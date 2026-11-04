//! Local transport only. Peer metadata is an admission check, never proof
//! that native authentication succeeded. Approval stays in the actual host.

use crate::{Error, Result};
use futures::io::{AsyncRead, AsyncWrite};
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

pub fn random_name() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| Error::Random)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

struct ListenerInner {
    listener: Option<platform::Listener>,
    directory: Option<PathBuf>,
}

impl Drop for ListenerInner {
    fn drop(&mut self) {
        self.listener.take();
        if let Some(directory) = &self.directory {
            let _ = std::fs::remove_file(directory.join("s"));
            let _ = std::fs::remove_dir(directory);
        }
    }
}

/// The shutdown of most processes is an exit, which runs no destructors, so
/// a listener's directory usually outlives it. Each new one clears up after
/// those of this account that nothing answers on any more.
#[cfg(unix)]
fn remove_abandoned() {
    let Ok(entries) = std::fs::read_dir("/tmp") else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let is_listener = name
            .to_str()
            .is_some_and(|name| local_name(&format!("/tmp/{name}/s")));
        if is_listener {
            remove_if_abandoned(&entry.path(), std::time::Duration::from_secs(60));
        }
    }
}

/// `settled` covers a listener between its bind and its listen.
#[cfg(unix)]
fn remove_if_abandoned(directory: &Path, settled: std::time::Duration) {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let socket = directory.join("s");
    let abandoned = std::fs::symlink_metadata(directory).is_ok_and(|metadata| {
        metadata.is_dir() && metadata.uid() == nix::unistd::geteuid().as_raw()
    }) && std::fs::symlink_metadata(&socket).is_ok_and(|metadata| {
        metadata.file_type().is_socket()
            && metadata
                .modified()
                .ok()
                .and_then(|time| time.elapsed().ok())
                .is_some_and(|age| age >= settled)
    }) && matches!(
        std::os::unix::net::UnixStream::connect(&socket),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused
    );
    if abandoned {
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_dir(directory);
    }
}

pub struct LocalListener {
    inner: Arc<ListenerInner>,
    name: String,
}

impl LocalListener {
    pub fn bind() -> Result<Self> {
        let suffix = random_name()?;
        #[cfg(unix)]
        let (name, directory) = {
            use std::os::unix::fs::DirBuilderExt;
            let _ = std::thread::Builder::new()
                .name("connect-stale-sockets".into())
                .spawn(remove_abandoned);
            let directory = PathBuf::from(format!("/tmp/tt-connect-{suffix}"));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&directory)
                .map_err(|_| Error::Unavailable)?;
            (
                directory.join("s").to_string_lossy().into_owned(),
                Some(directory),
            )
        };
        #[cfg(windows)]
        let (name, directory) = (format!(r"\\.\pipe\ThinkTerm.Connect.{suffix}"), None);
        let listener = match platform::bind(&name) {
            Ok(listener) => listener,
            Err(error) => {
                if let Some(directory) = directory {
                    let _ = std::fs::remove_dir(directory);
                }
                return Err(error);
            }
        };
        Ok(Self {
            inner: Arc::new(ListenerInner {
                listener: Some(listener),
                directory,
            }),
            name,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub async fn accept(&self) -> Result<LocalStream> {
        platform::accept(Arc::clone(&self.inner)).await
    }
}

#[derive(Clone)]
pub struct Peer {
    pid: u32,
}

impl Peer {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn is_packaged(&self, name: &str) -> bool {
        let current = match std::env::current_exe().and_then(std::fs::canonicalize) {
            Ok(path) => path,
            Err(_) => return false,
        };
        let expected = match current
            .parent()
            .map(|dir| dir.join(name))
            .and_then(|path| path.canonicalize().ok())
        {
            Some(path) => path,
            None => return false,
        };
        procinfo::LocalProcessInfo::executable_path(self.pid)
            .and_then(|path| path.canonicalize().ok())
            .is_some_and(|path| same_image(&path, &expected))
    }
}

fn same_image(actual: &Path, expected: &Path) -> bool {
    actual == expected
}

pub struct LocalStream {
    stream: platform::Stream,
    peer: Peer,
}

impl LocalStream {
    fn new(stream: platform::Stream) -> Result<Self> {
        let peer = platform::peer(&stream)?;
        if peer.pid == 0 {
            return Err(Error::Unavailable);
        }
        Ok(Self { stream, peer })
    }

    pub async fn connect(name: &str) -> Result<Self> {
        if !local_name(name) {
            return Err(Error::Protocol);
        }
        platform::connect(name).await
    }

    pub fn peer(&self) -> &Peer {
        &self.peer
    }
}

fn local_name(name: &str) -> bool {
    #[cfg(unix)]
    let suffix = name
        .strip_prefix("/tmp/tt-connect-")
        .and_then(|name| name.strip_suffix("/s"));
    #[cfg(windows)]
    let suffix = name.strip_prefix(r"\\.\pipe\ThinkTerm.Connect.");
    suffix.is_some_and(|suffix| {
        suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

impl std::fmt::Debug for LocalStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConnectLocalStream(..)")
    }
}

impl AsyncRead for LocalStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_read(cx, bytes)
    }
}

impl AsyncWrite for LocalStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_close(cx)
    }
}

impl Drop for LocalStream {
    fn drop(&mut self) {
        platform::discard(&self.stream);
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    pub type Listener = smol::Async<std::os::unix::net::UnixListener>;
    pub type Stream = smol::Async<std::os::unix::net::UnixStream>;

    pub fn bind(name: &str) -> Result<Listener> {
        Listener::bind(name).map_err(|_| Error::Unavailable)
    }

    pub async fn accept(listener: Arc<ListenerInner>) -> Result<LocalStream> {
        let (stream, _) = listener
            .listener
            .as_ref()
            .ok_or(Error::Unavailable)?
            .accept()
            .await
            .map_err(|_| Error::Unavailable)?;
        LocalStream::new(stream)
    }

    pub async fn connect(name: &str) -> Result<LocalStream> {
        LocalStream::new(
            Stream::connect(name)
                .await
                .map_err(|_| Error::Unavailable)?,
        )
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub fn peer(stream: &Stream) -> Result<Peer> {
        #[cfg(target_os = "macos")]
        let (uid, pid) = {
            use nix::sys::socket::{getsockopt, sockopt};
            (
                getsockopt(stream.get_ref(), sockopt::LocalPeerCred)
                    .map_err(|_| Error::Unavailable)?
                    .uid(),
                getsockopt(stream.get_ref(), sockopt::LocalPeerPid)
                    .map_err(|_| Error::Unavailable)?,
            )
        };
        #[cfg(target_os = "linux")]
        let (uid, pid) = {
            let credentials = nix::sys::socket::getsockopt(
                stream.get_ref(),
                nix::sys::socket::sockopt::PeerCredentials,
            )
            .map_err(|_| Error::Unavailable)?;
            (credentials.uid(), credentials.pid())
        };
        if uid != nix::unistd::geteuid().as_raw() {
            return Err(Error::Unavailable);
        }
        Ok(Peer {
            pid: u32::try_from(pid).map_err(|_| Error::Unavailable)?,
        })
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub fn peer(_: &Stream) -> Result<Peer> {
        Err(Error::Unavailable)
    }

    pub fn discard(_: &Stream) {}
}

#[cfg(windows)]
mod platform {
    use super::*;
    use interprocess::local_socket::{
        tokio::{Listener as NativeListener, Stream as NativeStream},
        traits::{
            tokio::{Listener as _, Stream as _},
            StreamCommon,
        },
        GenericFilePath, ListenerOptions, ToFsName,
    };
    use std::sync::OnceLock;
    use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};
    pub type Listener = NativeListener;
    pub type Stream = Compat<NativeStream>;

    fn runtime() -> Result<&'static tokio::runtime::Runtime> {
        static RUNTIME: OnceLock<std::result::Result<tokio::runtime::Runtime, ()>> =
            OnceLock::new();
        RUNTIME
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .max_blocking_threads(4)
                    .enable_io()
                    .enable_time()
                    .thread_name("connect-local-io")
                    .build()
                    .map_err(|_| ())
            })
            .as_ref()
            .map_err(|_| Error::Unavailable)
    }

    async fn join<T>(task: tokio::task::JoinHandle<T>) -> Result<T> {
        struct Abort<T>(tokio::task::JoinHandle<T>);
        impl<T> Drop for Abort<T> {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let mut task = Abort(task);
        (&mut task.0).await.map_err(|_| Error::Unavailable)
    }

    pub fn bind(name: &str) -> Result<Listener> {
        use interprocess::os::windows::{
            local_socket::ListenerOptionsExt, security_descriptor::SecurityDescriptor,
        };
        // No Everyone or anonymous ACE. Remote connections are refused by
        // Interprocess's named-pipe listener defaults.
        let sddl =
            widestring::U16CString::from_str("D:P(A;;GA;;;OW)").map_err(|_| Error::Unavailable)?;
        let security = SecurityDescriptor::deserialize(&sddl).map_err(|_| Error::Unavailable)?;
        let _runtime = runtime()?.enter();
        ListenerOptions::new()
            .name(
                name.to_fs_name::<GenericFilePath>()
                    .map_err(|_| Error::Protocol)?,
            )
            .security_descriptor(security)
            .create_tokio()
            .map_err(|_| Error::Unavailable)
    }

    pub async fn accept(listener: Arc<ListenerInner>) -> Result<LocalStream> {
        let task = runtime()?.spawn(async move {
            listener
                .listener
                .as_ref()
                .ok_or(Error::Unavailable)?
                .accept()
                .await
                .map_err(|_| Error::Unavailable)
        });
        LocalStream::new(join(task).await??.compat())
    }

    pub async fn connect(name: &str) -> Result<LocalStream> {
        let name = name.to_string();
        let task = runtime()?.spawn(async move {
            NativeStream::connect(
                name.as_str()
                    .to_fs_name::<GenericFilePath>()
                    .map_err(|_| Error::Protocol)?,
            )
            .await
            .map_err(|_| Error::Unavailable)
        });
        LocalStream::new(join(task).await??.compat())
    }

    pub fn peer(stream: &Stream) -> Result<Peer> {
        let credentials = stream
            .get_ref()
            .peer_creds()
            .map_err(|_| Error::Unavailable)?;
        Ok(Peer {
            pid: credentials.pid().ok_or(Error::Unavailable)?,
        })
    }

    pub fn discard(stream: &Stream) {
        let NativeStream::NamedPipe(pipe) = stream.get_ref();
        // Cancellation must not leave a FlushFileBuffers job in a background
        // queue. Successful replies use an explicit protocol acknowledgement.
        pipe.inner().assume_flushed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    use futures::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn discovery_cannot_redirect_to_an_arbitrary_or_remote_pipe() {
        let listener_name = if cfg!(unix) {
            "/tmp/tt-connect-0123456789abcdef0123456789abcdef/s"
        } else {
            r"\\.\pipe\ThinkTerm.Connect.0123456789abcdef0123456789abcdef"
        };
        assert!(local_name(listener_name));
        for name in [
            r"\\server-a\pipe\example",
            "/tmp/example",
            "/tmp/tt-connect-../s",
            "",
        ] {
            assert!(!local_name(name));
        }
    }
    #[cfg(unix)]
    #[test]
    fn only_a_socket_nothing_answers_on_counts_as_abandoned() {
        use std::time::Duration;
        let listener = LocalListener::bind().unwrap();
        let live = PathBuf::from(listener.name());
        let live = live.parent().unwrap();
        remove_if_abandoned(live, Duration::ZERO);
        assert!(live.join("s").exists());

        let stale = PathBuf::from(format!("/tmp/tt-connect-{}", random_name().unwrap()));
        std::fs::create_dir(&stale).unwrap();
        drop(std::os::unix::net::UnixListener::bind(stale.join("s")).unwrap());
        remove_if_abandoned(&stale, Duration::from_secs(600));
        assert!(stale.join("s").exists(), "too recent to be sure");
        remove_if_abandoned(&stale, Duration::ZERO);
        assert!(!stale.exists());
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn local_transport_binds_kernel_peer_identity_and_releases_its_name() {
        smol::block_on(async {
            let listener = LocalListener::bind().unwrap();
            let name = listener.name().to_string();
            let (accepted, connected) =
                futures::join!(listener.accept(), LocalStream::connect(&name));
            let mut accepted = accepted.unwrap();
            let mut connected = connected.unwrap();
            assert_eq!(accepted.peer().pid(), std::process::id());
            assert_eq!(connected.peer().pid(), std::process::id());
            connected.write_all(b"hello").await.unwrap();
            let mut bytes = [0; 5];
            accepted.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"hello");
            drop(connected);
            drop(accepted);
            drop(listener);
            #[cfg(unix)]
            assert!(!Path::new(&name).exists());
        });
    }

    #[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
    #[test]
    fn unsupported_unix_transport_refuses_peers_and_releases_its_name() {
        smol::block_on(async {
            let listener = LocalListener::bind().unwrap();
            let name = listener.name().to_string();
            let (accepted, connected) =
                futures::join!(listener.accept(), LocalStream::connect(&name));
            assert!(matches!(accepted, Err(Error::Unavailable)));
            assert!(matches!(connected, Err(Error::Unavailable)));
            drop(listener);
            assert!(!Path::new(&name).exists());
        });
    }
}
