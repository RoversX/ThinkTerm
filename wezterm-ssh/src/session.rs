use crate::auth::*;
use crate::authentication::AuthenticationInfo;
use crate::config::ConfigMap;
use crate::host::*;
use crate::pty::*;
use crate::sessioninner::*;
use crate::sftp::{Sftp, SftpRequest};
use filedescriptor::{socketpair, FileDescriptor};
use portable_pty::PtySize;
use smol::channel::{bounded, Receiver, Sender};
use socket2::Socket;
use std::collections::HashMap;
use std::io::Write;
use std::net::Shutdown;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum SessionEvent {
    Banner(Option<String>),
    HostVerify(HostVerificationEvent),
    Authenticate(AuthenticationEvent),
    HostVerificationFailed(HostVerificationFailed),
    Error(String),
    /// Like `Error`, for a sign-in that ended because the server turned
    /// down an answer to one of its prompts. Giving the same answer again
    /// ends the same way, and servers count such attempts.
    AuthenticationRefused(String),
    Authenticated,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionSender {
    pub tx: Sender<SessionRequest>,
    pub pipe: Arc<Mutex<FileDescriptor>>,
}

impl SessionSender {
    fn post_send(&self) {
        let mut pipe = self.pipe.lock().unwrap();
        let _ = pipe.write(b"x");
    }

    pub fn try_send(&self, event: SessionRequest) -> anyhow::Result<()> {
        self.tx.try_send(event)?;
        self.post_send();
        Ok(())
    }

    pub(crate) fn try_send_request(
        &self,
        event: SessionRequest,
    ) -> Result<(), smol::channel::TrySendError<SessionRequest>> {
        self.tx.try_send(event)?;
        self.post_send();
        Ok(())
    }

    pub async fn send(&self, event: SessionRequest) -> anyhow::Result<()> {
        self.tx.send(event).await?;
        self.post_send();
        Ok(())
    }
}

#[derive(thiserror::Error, Debug)]
#[error("SSH session is dead")]
pub struct DeadSession;

#[derive(Debug)]
pub(crate) enum SessionRequest {
    NewPty(NewPty, Sender<anyhow::Result<(SshPty, SshChildProcess)>>),
    ResizePty(ResizePty, Option<Sender<anyhow::Result<()>>>),
    Exec(Exec, Sender<anyhow::Result<ExecResult>>),
    DirectTcpip(DirectTcpip, Sender<anyhow::Result<Socket>>),
    Sftp(SftpRequest),
    SignalChannel(SignalChannel),
    SessionDropped,
}

#[derive(Debug)]
pub(crate) struct SignalChannel {
    pub channel: ChannelId,
    pub signame: &'static str,
}

#[derive(Debug)]
pub(crate) struct Exec {
    pub command_line: String,
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug)]
pub(crate) struct DirectTcpip {
    pub host: String,
    pub port: u16,
}

#[derive(Clone)]
pub struct Session {
    tx: SessionSender,
    shutdown: Arc<SessionShutdown>,
    authentication: Arc<OnceLock<AuthenticationInfo>>,
}

/// Shutting down a duplicate interrupts even a backend blocked in its
/// handshake or a channel request. Sending a request alone cannot do that.
#[derive(Default)]
pub(crate) struct SessionShutdown {
    stopped: AtomicBool,
    socket: Mutex<Option<Socket>>,
    /// The sessions to the jump hosts this one is reached through: with
    /// this one gone they carry nothing, and one still signing in would
    /// otherwise keep this one's caller waiting for nothing.
    linked: Mutex<Vec<Arc<SessionShutdown>>>,
}

impl SessionShutdown {
    pub(crate) fn check(&self) -> anyhow::Result<()> {
        if self.stopped.load(Ordering::Acquire) {
            anyhow::bail!("SSH session was shut down");
        }
        Ok(())
    }

    pub(crate) fn watch(&self, socket: &Socket) -> anyhow::Result<()> {
        let mut watched = self.socket.lock().unwrap_or_else(|e| e.into_inner());
        if self.check().is_err() {
            let _ = socket.shutdown(Shutdown::Both);
            anyhow::bail!("SSH session was shut down");
        }
        *watched = Some(socket.try_clone()?);
        Ok(())
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(socket) = self.socket.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        let linked = std::mem::take(&mut *self.linked.lock().unwrap_or_else(|e| e.into_inner()));
        for other in linked {
            other.stop();
        }
    }

    /// Stops `other` along with this one, or at once if this one has been.
    pub(crate) fn link(&self, other: Arc<SessionShutdown>) {
        let mut linked = self.linked.lock().unwrap_or_else(|e| e.into_inner());
        if self.stopped.load(Ordering::Acquire) {
            drop(linked);
            other.stop();
        } else {
            linked.push(other);
        }
    }

    pub(crate) fn clear(&self) {
        self.socket.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.tx.try_send(SessionRequest::SessionDropped).ok();
        log::trace!("Drop Session");
    }
}

impl Session {
    /// Available before `SessionEvent::Authenticated` is delivered. It is
    /// populated only by this session's handshake and never by configuration
    /// or a caller-supplied authentication event. It is not a live grant.
    pub fn authentication_info(&self) -> Option<&AuthenticationInfo> {
        self.authentication.get()
    }

    /// Abort this session, including open channels and pending requests.
    /// All clones refer to the same connection; ordinary drop still lets
    /// its existing channels finish.
    pub fn shutdown(&self) {
        self.shutdown.stop();
        self.tx.tx.close();
        self.tx.post_send();
    }

    pub fn connect(config: ConfigMap) -> anyhow::Result<(Self, Receiver<SessionEvent>)> {
        Self::start(config, None)
    }

    /// Like [`Session::connect`], over `transport` rather than a connection
    /// of its own: one a jump host carries to the host.
    pub(crate) fn connect_over(
        config: ConfigMap,
        transport: Socket,
    ) -> anyhow::Result<(Self, Receiver<SessionEvent>)> {
        Self::start(config, Some(transport))
    }

    pub(crate) fn shutdown_handle(&self) -> Arc<SessionShutdown> {
        Arc::clone(&self.shutdown)
    }

    fn start(
        config: ConfigMap,
        transport: Option<Socket>,
    ) -> anyhow::Result<(Self, Receiver<SessionEvent>)> {
        // Room for what a sign-in says unasked -- banners, mostly -- before
        // whoever connects starts reading: a session reached through jump
        // hosts passes theirs on too, up to `MAX_JUMP_HOSTS` sessions' worth,
        // and one sent with no room left would end the sign-in.
        let (tx_event, rx_event) = bounded(64);
        let (tx_req, rx_req) = bounded(8);
        let (mut sender_write, mut sender_read) = socketpair()?;
        sender_write.set_non_blocking(true)?;
        sender_read.set_non_blocking(true)?;

        let session_sender = SessionSender {
            tx: tx_req,
            pipe: Arc::new(Mutex::new(sender_write)),
        };

        let keep_alive = config.get("serveraliveinterval").and_then(|value| {
            let seconds: u64 = value.parse().ok()?;
            if seconds == 0 {
                None
            } else {
                Some(Duration::from_secs(seconds))
            }
        });

        let now = Instant::now();
        let shutdown = Arc::new(SessionShutdown::default());
        let authentication = Arc::new(OnceLock::new());

        let mut inner = SessionInner {
            config,
            tx_event,
            rx_req,
            channels: HashMap::new(),
            files: HashMap::new(),
            dirs: HashMap::new(),
            next_channel_id: 1,
            next_file_id: 1,
            sender_read,
            session_was_dropped: false,
            shown_accept_env_error: false,
            last_keep_alive: now,
            keep_alive,
            shutdown: Arc::clone(&shutdown),
            authentication: Arc::clone(&authentication),
            transport,
        };
        std::thread::spawn(move || inner.run());
        Ok((
            Self {
                tx: session_sender,
                shutdown,
                authentication,
            },
            rx_event,
        ))
    }

    pub async fn request_pty(
        &self,
        term: &str,
        size: PtySize,
        command_line: Option<&str>,
        env: Option<HashMap<String, String>>,
    ) -> anyhow::Result<(SshPty, SshChildProcess)> {
        let (reply, rx) = bounded(1);
        self.tx
            .send(SessionRequest::NewPty(
                NewPty {
                    term: term.to_string(),
                    size,
                    command_line: command_line.map(|s| s.to_string()),
                    env,
                },
                reply,
            ))
            .await
            .map_err(|_| DeadSession)?;
        let (mut ssh_pty, mut child) = rx.recv().await??;
        ssh_pty.tx.replace(self.tx.clone());
        child.tx.replace(self.tx.clone());
        Ok((ssh_pty, child))
    }

    pub async fn exec(
        &self,
        command_line: &str,
        env: Option<HashMap<String, String>>,
    ) -> anyhow::Result<ExecResult> {
        let (reply, rx) = bounded(1);
        self.tx
            .send(SessionRequest::Exec(
                Exec {
                    command_line: command_line.to_string(),
                    env,
                },
                reply,
            ))
            .await
            .map_err(|_| DeadSession)?;
        let mut exec = rx.recv().await??;
        exec.child.tx.replace(self.tx.clone());
        Ok(exec)
    }

    /// Has the server open a TCP connection to `host:port` for us, as
    /// `ssh -W` does: a direct-tcpip channel, given back as a socket that
    /// passes along what is written to it and what comes back. It is how a
    /// jump host carries the connection to the host beyond it.
    pub async fn open_direct_tcpip(&self, host: &str, port: u16) -> anyhow::Result<Socket> {
        let (reply, rx) = bounded(1);
        self.tx
            .send(SessionRequest::DirectTcpip(
                DirectTcpip {
                    host: host.to_string(),
                    port,
                },
                reply,
            ))
            .await
            .map_err(|_| DeadSession)?;
        rx.recv().await?
    }

    /// Creates a new reference to the sftp channel for filesystem operations
    ///
    /// ### Note
    ///
    /// This does not actually initialize the sftp subsystem and only provides
    /// a reference to a means to perform sftp operations. Upon requesting the
    /// first sftp operation, the sftp subsystem will be initialized.
    pub fn sftp(&self) -> Sftp {
        Sftp {
            tx: self.tx.clone(),
        }
    }
}

#[derive(Debug)]
pub struct ExecResult {
    pub stdin: FileDescriptor,
    pub stdout: FileDescriptor,
    pub stderr: FileDescriptor,
    pub child: SshChildProcess,
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use std::io::Read;
    #[cfg(windows)]
    use std::net::{Ipv4Addr, TcpListener, TcpStream as Stream};
    #[cfg(unix)]
    use std::os::unix::net::UnixStream as Stream;

    #[cfg(unix)]
    fn pair() -> (Socket, Stream) {
        let (socket, peer) = Stream::pair().unwrap();
        (std::os::fd::OwnedFd::from(socket).into(), peer)
    }

    #[cfg(windows)]
    fn pair() -> (Socket, Stream) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let peer = Stream::connect(listener.local_addr().unwrap()).unwrap();
        let (socket, _) = listener.accept().unwrap();
        (socket.into(), peer)
    }

    #[test]
    fn shutdown_interrupts_a_blocked_transport_read() {
        let (socket, _peer) = pair();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let shutdown = SessionShutdown::default();
        shutdown.watch(&socket).unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            #[cfg(unix)]
            let mut stream = Stream::from(std::os::fd::OwnedFd::from(socket));
            #[cfg(windows)]
            let mut stream = Stream::from(socket);
            sent.send(stream.read(&mut [0u8; 1])).unwrap();
        });
        shutdown.stop();
        let result = received.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(result, Ok(0)), "{:?}", result);
        reader.join().unwrap();
    }

    #[test]
    fn shutdown_rejects_a_transport_that_arrives_late() {
        let shutdown = SessionShutdown::default();
        shutdown.stop();
        let (socket, mut peer) = pair();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert!(shutdown.watch(&socket).is_err());
        assert_eq!(peer.read(&mut [0u8; 1]).unwrap(), 0);
    }
}
