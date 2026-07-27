use crate::workspace_threads::{RemoteFilesSource, RemoteFilesTarget};
use config::SshDomain;
use smol::channel::Sender;
use smol::io::AsyncReadExt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use wezterm_ssh::{Session, SessionEvent, SftpChannelError, SftpError};

pub(crate) const REMOTE_FILE_TREE_ROW_LIMIT: usize = 2_000;

type RemoteFuture<T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'static>>;

/// A remote Unix path.  It never uses the host OS path implementation, so a
/// Windows client cannot introduce `\` while joining SFTP paths.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemotePath(String);

impl RemotePath {
    pub(crate) fn from_server_absolute(path: &str) -> Result<Self, String> {
        if !path.starts_with('/') {
            return Err(format!("SFTP returned a non-absolute path: {path}"));
        }
        if path.as_bytes().contains(&0) {
            return Err("remote path contains NUL".to_string());
        }
        let normalized = if path.len() > 1 {
            path.trim_end_matches('/').to_string()
        } else {
            "/".to_string()
        };
        Ok(Self(normalized))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    pub(crate) fn extension(&self) -> Option<&str> {
        let name = self.file_name();
        let (stem, extension) = name.rsplit_once('.')?;
        (!stem.is_empty() && !extension.is_empty()).then_some(extension)
    }

    pub(crate) fn join_name(&self, name: &str) -> Result<Self, String> {
        if name.is_empty()
            || matches!(name, "." | "..")
            || name.contains('/')
            || name.as_bytes().contains(&0)
        {
            return Err(format!("invalid SFTP directory entry name: {name:?}"));
        }
        let joined = if self.0 == "/" {
            format!("/{name}")
        } else {
            format!("{}/{name}", self.0)
        };
        Ok(Self(joined))
    }

    pub(crate) fn is_descendant_of(&self, parent: &Self) -> bool {
        if parent.0 == "/" {
            self.0.len() > 1 && self.0.starts_with('/')
        } else {
            self.0
                .strip_prefix(&parent.0)
                .is_some_and(|suffix| suffix.starts_with('/'))
        }
    }
}

/// Whether `requested` needs the remote home resolved before it can be used.
/// Absolute roots do not: asking the server to canonicalize `.` first is both
/// pointless and a needless failure point, since a server that cannot answer
/// that request would break a project whose path was fully specified.
fn requested_root_needs_home(requested: &str) -> bool {
    requested == "~" || requested.starts_with("~/")
}

/// The spellings a server may accept when asked to canonicalize the session's
/// starting directory. SFTP has no `~`; `.` is the usual spelling, but not
/// every server answers it, so the empty path is tried before giving up.
/// The liveness probe MUST accept the same spellings as root resolution — a
/// stricter probe declares sessions dead that the panel could actually use,
/// forcing a needless redial on every idle reacquire.
const REMOTE_HOME_SPELLINGS: [&str; 2] = [".", ""];

/// Try each home spelling in order, returning the first success or every
/// failure (in spelling order). Generic over the canonicalize call so the
/// fallback contract stays testable without a server.
async fn canonicalize_remote_home<T, E, F, Fut>(mut canonicalize: F) -> Result<T, Vec<E>>
where
    F: FnMut(&'static str) -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut failures = Vec::new();
    for spelling in REMOTE_HOME_SPELLINGS {
        match canonicalize(spelling).await {
            Ok(value) => return Ok(value),
            Err(err) => failures.push(err),
        }
    }
    Err(failures)
}

/// Validate a stored project path for use as a browse root. Kept separate from
/// [`RemotePath::from_server_absolute`] so the diagnostics distinguish "the
/// server answered with something odd" from "this project's stored path cannot
/// be browsed", and so traversal components never reach the server.
fn remote_root_from_absolute(requested: &str) -> Result<RemotePath, String> {
    if !requested.starts_with('/') {
        return Err(format!(
            "Unsupported remote project path {requested:?}: expected an absolute path or ~"
        ));
    }
    if requested.split('/').any(|component| component == "..") {
        return Err(format!(
            "Unsupported remote project path {requested:?}: `..` is not allowed"
        ));
    }
    RemotePath::from_server_absolute(requested)
}

/// Flatten an error and everything it wraps into one line. libssh and ssh2
/// failures surface through `SftpChannelError` as the near-useless
/// "Library-specific error", with the real reason one or more levels down.
fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = err.source();
    while let Some(inner) = source {
        parts.push(inner.to_string());
        source = inner.source();
    }
    parts.join(": ")
}

/// The deepest, most specific message in the chain — the part worth putting in
/// front of the user when the full chain is too long for a sidebar.
fn error_summary(err: &(dyn std::error::Error + 'static)) -> String {
    let mut deepest = err.to_string();
    let mut source = err.source();
    while let Some(inner) = source {
        let text = inner.to_string();
        if !text.is_empty() {
            deepest = text;
        }
        source = inner.source();
    }
    deepest
}

fn resolve_requested_root(home: RemotePath, requested: &str) -> Result<RemotePath, String> {
    if requested == "~" {
        return Ok(home);
    }
    if let Some(suffix) = requested.strip_prefix("~/") {
        let mut path = home;
        for component in suffix.split('/') {
            if component.is_empty() || component == "." {
                continue;
            }
            path = path.join_name(component)?;
        }
        return Ok(path);
    }
    RemotePath::from_server_absolute(requested)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteFileKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteFileEntry {
    pub path: RemotePath,
    pub name: String,
    pub kind: RemoteFileKind,
    pub size: Option<u64>,
}

impl RemoteFileEntry {
    pub(crate) fn is_directory(&self) -> bool {
        self.kind == RemoteFileKind::Directory
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteDirectoryListing {
    pub entries: Vec<RemoteFileEntry>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteFileBytes {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

pub(crate) trait RemoteFileBackend: Send + Sync {
    fn resolve_root(&self, requested: String) -> RemoteFuture<RemotePath>;
    fn list_directory(
        &self,
        path: RemotePath,
        limit: usize,
    ) -> RemoteFuture<RemoteDirectoryListing>;
    fn read_file(&self, path: RemotePath, limit: usize) -> RemoteFuture<RemoteFileBytes>;

    /// Cheapest possible round-trip, used to confirm a cached connection is
    /// still alive before it is handed out again. The default is a no-op so
    /// test doubles opt in explicitly.
    fn probe(&self) -> RemoteFuture<()> {
        Box::pin(async { Ok(()) })
    }
}

pub(crate) trait RemoteFileConnector: Send + Sync {
    fn connect(&self, config: SshDomain) -> RemoteFuture<Arc<dyn RemoteFileBackend>>;
}

pub(crate) fn remote_connection_key(source_key: &str, config: &SshDomain) -> String {
    let mut hasher = DefaultHasher::new();
    source_key.hash(&mut hasher);
    config.remote_address.hash(&mut hasher);
    config.username.hash(&mut hasher);
    config.no_agent_auth.hash(&mut hasher);
    format!("{:?}", config.ssh_backend).hash(&mut hasher);
    let mut options = config.ssh_option.iter().collect::<Vec<_>>();
    options.sort_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.cmp(b.1)));
    options.hash(&mut hasher);
    // Include credentials in the fingerprint without retaining or logging
    // another copy of the secret.
    config.stored_password.hash(&mut hasher);
    format!("{source_key}:{:016x}", hasher.finish())
}

struct SftpRemoteFileBackend {
    // Sftp only retains a request sender.  Keep the Session handle alive for
    // the lifetime of this independent file connection.
    _session: Session,
    sftp: wezterm_ssh::Sftp,
}

impl RemoteFileBackend for SftpRemoteFileBackend {
    fn resolve_root(&self, requested: String) -> RemoteFuture<RemotePath> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            if !requested_root_needs_home(&requested) {
                // Already absolute (or invalid): no server round-trip needed.
                return remote_root_from_absolute(&requested);
            }

            // The home directory is whatever the server canonicalizes the
            // session's starting directory to; see REMOTE_HOME_SPELLINGS.
            match canonicalize_remote_home(|spelling| {
                let sftp = sftp.clone();
                async move { sftp.canonicalize(spelling.to_string()).await }
            })
            .await
            {
                Ok(home) => resolve_requested_root(
                    RemotePath::from_server_absolute(home.as_str())?,
                    &requested,
                ),
                Err(errors) => {
                    let mut failures = Vec::new();
                    for (spelling, err) in REMOTE_HOME_SPELLINGS.iter().zip(&errors) {
                        log::warn!(
                            "remote files: sftp canonicalize({spelling:?}) failed: {}",
                            error_chain(err)
                        );
                        failures.push(error_summary(err));
                    }
                    let detail = failures.last().cloned().unwrap_or_default();
                    log::error!("remote files: unable to resolve remote home: {failures:?}");
                    Err(format!("Can't read the remote home directory: {detail}"))
                }
            }
        })
    }

    fn list_directory(
        &self,
        path: RemotePath,
        limit: usize,
    ) -> RemoteFuture<RemoteDirectoryListing> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            let dir = sftp
                .open_dir(path.as_str().to_string())
                .await
                .map_err(|err| format!("Unable to open {}: {err}", path.as_str()))?;
            let mut entries = Vec::with_capacity(limit.min(256));
            let mut truncated = false;
            loop {
                // The raw API reports normal end-of-directory as an error.
                // Preserve other errors so a dead shared connection can be
                // invalidated rather than appearing as a partial directory.
                let (raw_name, metadata) = match dir.read_dir().await {
                    Ok(entry) => entry,
                    Err(err) if is_sftp_directory_eof(&err) => break,
                    Err(err) => {
                        return Err(format!(
                            "Unable to read directory {}: {err:#}",
                            path.as_str()
                        ))
                    }
                };
                let name = raw_name.as_str();
                if matches!(name, "." | "..") {
                    continue;
                }
                let Ok(entry_path) = path.join_name(name) else {
                    continue;
                };
                if entries.len() >= limit {
                    truncated = true;
                    break;
                }
                let kind = if metadata.is_dir() {
                    RemoteFileKind::Directory
                } else if metadata.is_file() {
                    RemoteFileKind::File
                } else if metadata.is_symlink() {
                    RemoteFileKind::Symlink
                } else {
                    RemoteFileKind::Other
                };
                entries.push(RemoteFileEntry {
                    path: entry_path,
                    name: name.to_string(),
                    kind,
                    size: metadata.size,
                });
            }
            entries.sort_by(|a, b| {
                b.is_directory()
                    .cmp(&a.is_directory())
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                    .then_with(|| a.name.cmp(&b.name))
            });
            Ok(RemoteDirectoryListing { entries, truncated })
        })
    }

    fn probe(&self) -> RemoteFuture<()> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            // Same spellings as resolve_root: a server that only answers the
            // empty path is alive, and declaring it dead here would force a
            // redial on every idle reacquire.
            match canonicalize_remote_home(|spelling| {
                let sftp = sftp.clone();
                async move { sftp.canonicalize(spelling.to_string()).await }
            })
            .await
            {
                Ok(_) => Ok(()),
                Err(errors) => Err(errors
                    .last()
                    .map(|err| error_chain(err))
                    .unwrap_or_default()),
            }
        })
    }

    fn read_file(&self, path: RemotePath, limit: usize) -> RemoteFuture<RemoteFileBytes> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            let file = sftp
                .open(path.as_str().to_string())
                .await
                .map_err(|err| format!("Unable to open {}: {err}", path.as_str()))?;
            let mut bytes = Vec::with_capacity(limit.saturating_add(1).min(256 * 1024));
            let mut limited = file.take(limit.saturating_add(1) as u64);
            limited
                .read_to_end(&mut bytes)
                .await
                .map_err(|err| format!("Unable to read {}: {err}", path.as_str()))?;
            let truncated = bytes.len() > limit;
            if truncated {
                bytes.truncate(limit);
            }
            Ok(RemoteFileBytes { bytes, truncated })
        })
    }
}

fn is_sftp_directory_eof(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let channel_eof = match cause.downcast_ref::<SftpChannelError>() {
            Some(SftpChannelError::Sftp(SftpError::Eof)) => true,
            Some(SftpChannelError::FileIo(io)) => io.kind() == std::io::ErrorKind::UnexpectedEof,
            _ => false,
        };
        channel_eof
            || cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof)
    })
}

#[derive(Default)]
pub(crate) struct SshRemoteFileConnector;

impl RemoteFileConnector for SshRemoteFileConnector {
    fn connect(&self, config: SshDomain) -> RemoteFuture<Arc<dyn RemoteFileBackend>> {
        Box::pin(async move {
            let ssh_config =
                mux::ssh::ssh_domain_to_ssh_config(&config).map_err(|err| format!("{err:#}"))?;
            let (session, events) = Session::connect(ssh_config)
                .map_err(|err| format!("Unable to connect to SSH server: {err:#}"))?;
            let mut password = config
                .stored_password
                .filter(|password| !password.is_empty());
            while let Ok(event) = events.recv().await {
                match event {
                    SessionEvent::Banner(_) => {}
                    SessionEvent::HostVerify(verify) => {
                        return Err(format!(
                            "SSH host verification requires interactive confirmation: {}",
                            verify.message
                        ));
                    }
                    SessionEvent::Authenticate(auth) => {
                        let mut answers = Vec::with_capacity(auth.prompts.len());
                        for prompt in &auth.prompts {
                            if prompt.echo {
                                return Err(
                                    "SSH authentication requires an interactive prompt".to_string()
                                );
                            }
                            let Some(stored) = password.take() else {
                                return Err(
                                    "SSH authentication requires a saved password".to_string()
                                );
                            };
                            answers.push(stored);
                        }
                        auth.answer(answers).await.map_err(|err| {
                            format!("Unable to answer SSH authentication prompt: {err:#}")
                        })?;
                    }
                    SessionEvent::HostVerificationFailed(failed) => {
                        return Err(format!("SSH host verification failed: {failed}"));
                    }
                    SessionEvent::Error(err) => return Err(format!("SSH error: {err}")),
                    SessionEvent::Authenticated => {
                        let backend: Arc<dyn RemoteFileBackend> = Arc::new(SftpRemoteFileBackend {
                            sftp: session.sftp(),
                            _session: session,
                        });
                        return Ok(backend);
                    }
                }
            }
            Err("SSH authentication did not complete".to_string())
        })
    }
}

enum ManagedConnection {
    Connecting {
        waiters: Vec<Sender<Result<(), String>>>,
    },
    /// A previously-idle entry whose liveness one acquirer is verifying.
    /// Everyone else queues: handing the backend out before the verdict would
    /// let a caller run operations on a session about to be declared dead —
    /// and that caller's failure report could then tear down the healthy
    /// replacement the prober dials.
    Probing {
        connection_id: u64,
        backend: Arc<dyn RemoteFileBackend>,
        idle_generation: u64,
        waiters: Vec<Sender<Result<(), String>>>,
    },
    Ready {
        connection_id: u64,
        backend: Arc<dyn RemoteFileBackend>,
        leases: usize,
        idle_generation: u64,
        idle_since: Option<Instant>,
    },
}

struct RemoteConnectionManagerInner {
    entries: HashMap<String, ManagedConnection>,
    idle_timeout: Duration,
    next_connection_id: u64,
}

pub(crate) struct RemoteConnectionManager {
    connector: Arc<dyn RemoteFileConnector>,
    inner: Mutex<RemoteConnectionManagerInner>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteAcquireError {
    NotConnected,
    Failed(String),
}

pub(crate) struct RemoteConnectionLease {
    key: String,
    backend: Arc<dyn RemoteFileBackend>,
    manager: Weak<RemoteConnectionManager>,
    connection_id: u64,
}

impl RemoteConnectionLease {
    pub(crate) fn backend(&self) -> Arc<dyn RemoteFileBackend> {
        Arc::clone(&self.backend)
    }

    pub(crate) fn connection_key(&self) -> &str {
        &self.key
    }

    /// Identity of the pooled connection behind this lease. Failure reports
    /// must carry it so they can only invalidate the connection they actually
    /// ran on, never a replacement dialled under the same key since.
    pub(crate) fn connection_id(&self) -> u64 {
        self.connection_id
    }

    pub(crate) fn operation_lease(&self) -> Option<RemoteConnectionLease> {
        self.manager
            .upgrade()
            .and_then(|manager| manager.retain(&self.key, self.connection_id))
    }
}

impl Drop for RemoteConnectionLease {
    fn drop(&mut self) {
        if let Some(manager) = self.manager.upgrade() {
            manager.release(&self.key, self.connection_id);
        }
    }
}

impl RemoteConnectionManager {
    fn new(
        connector: Arc<dyn RemoteFileConnector>,
        idle_timeout: Duration,
    ) -> Arc<RemoteConnectionManager> {
        Arc::new(Self {
            connector,
            inner: Mutex::new(RemoteConnectionManagerInner {
                entries: HashMap::new(),
                idle_timeout,
                next_connection_id: 0,
            }),
        })
    }

    pub(crate) async fn acquire(
        self: &Arc<Self>,
        key: String,
        config: SshDomain,
        allow_connect: bool,
    ) -> Result<RemoteConnectionLease, RemoteAcquireError> {
        enum Pending {
            Dial,
            Wait(smol::channel::Receiver<Result<(), String>>),
            Probe(Arc<dyn RemoteFileBackend>, u64),
        }
        loop {
            let pending = {
                let mut inner = self.inner.lock().unwrap();
                match inner.entries.get_mut(&key) {
                    Some(ManagedConnection::Ready {
                        connection_id,
                        backend,
                        leases,
                        idle_generation,
                        idle_since,
                    }) => {
                        if idle_since.is_none() {
                            // Actively leased: known good, hand it out.
                            *leases = leases.saturating_add(1);
                            *idle_generation = idle_generation.wrapping_add(1);
                            return Ok(RemoteConnectionLease {
                                key: key.clone(),
                                backend: Arc::clone(backend),
                                manager: Arc::downgrade(self),
                                connection_id: *connection_id,
                            });
                        }
                        // A connection that sat with no leases carried no
                        // traffic, so the server or a NAT may have reaped it
                        // without us noticing. Verify before handing it out,
                        // and park later acquires until the verdict is in.
                        let connection_id = *connection_id;
                        let backend = Arc::clone(backend);
                        let idle_generation = *idle_generation;
                        inner.entries.insert(
                            key.clone(),
                            ManagedConnection::Probing {
                                connection_id,
                                backend: Arc::clone(&backend),
                                idle_generation,
                                waiters: Vec::new(),
                            },
                        );
                        Pending::Probe(backend, connection_id)
                    }
                    Some(ManagedConnection::Connecting { waiters })
                    | Some(ManagedConnection::Probing { waiters, .. }) => {
                        let (tx, rx) = smol::channel::bounded(1);
                        waiters.push(tx);
                        Pending::Wait(rx)
                    }
                    None if !allow_connect => return Err(RemoteAcquireError::NotConnected),
                    None => {
                        inner.entries.insert(
                            key.clone(),
                            ManagedConnection::Connecting { waiters: vec![] },
                        );
                        Pending::Dial
                    }
                }
            };

            match pending {
                Pending::Probe(backend, connection_id) => {
                    let probe_result = backend.probe().await;
                    let (waiters, lease) = {
                        let mut inner = self.inner.lock().unwrap();
                        match inner.entries.get_mut(&key) {
                            Some(ManagedConnection::Probing {
                                connection_id: current,
                                backend,
                                idle_generation,
                                waiters,
                            }) if *current == connection_id => {
                                let waiters = std::mem::take(waiters);
                                match &probe_result {
                                    Ok(()) => {
                                        let backend = Arc::clone(backend);
                                        let idle_generation = idle_generation.wrapping_add(1);
                                        inner.entries.insert(
                                            key.clone(),
                                            ManagedConnection::Ready {
                                                connection_id,
                                                backend: Arc::clone(&backend),
                                                leases: 1,
                                                idle_generation,
                                                idle_since: None,
                                            },
                                        );
                                        (
                                            waiters,
                                            Some(RemoteConnectionLease {
                                                key: key.clone(),
                                                backend,
                                                manager: Arc::downgrade(self),
                                                connection_id,
                                            }),
                                        )
                                    }
                                    Err(_) => {
                                        inner.entries.remove(&key);
                                        (waiters, None)
                                    }
                                }
                            }
                            // The entry moved on without us; whatever owns the
                            // key now decides the next iteration.
                            _ => (Vec::new(), None),
                        }
                    };
                    // Waiters re-examine the map rather than trusting the
                    // verdict second-hand: on success they find Ready, on
                    // failure they find nothing (or the prober's redial).
                    for waiter in waiters {
                        let _ = waiter.try_send(Ok(()));
                    }
                    match probe_result {
                        Ok(()) => {
                            if let Some(lease) = lease {
                                return Ok(lease);
                            }
                            continue;
                        }
                        Err(err) => {
                            // Dead after idling: it is already gone from the
                            // map; dial again on the next turn of the loop
                            // rather than surfacing a failure the user would
                            // have to retry by hand.
                            log::warn!(
                                "remote files: cached connection failed its liveness probe, \
                                 reconnecting: {err}"
                            );
                            continue;
                        }
                    }
                }
                Pending::Wait(waiter) => match waiter.recv().await {
                    Ok(Ok(())) => continue,
                    Ok(Err(err)) => return Err(RemoteAcquireError::Failed(err)),
                    Err(_) => {
                        return Err(RemoteAcquireError::Failed(
                            "Remote Files connection was canceled".to_string(),
                        ))
                    }
                },
                Pending::Dial => {}
            }

            let result = self.connector.connect(config.clone()).await;
            let waiters = {
                let mut inner = self.inner.lock().unwrap();
                let waiters = match inner.entries.remove(&key) {
                    Some(ManagedConnection::Connecting { waiters }) => waiters,
                    _ => Vec::new(),
                };
                if let Ok(backend) = &result {
                    inner.next_connection_id = inner.next_connection_id.wrapping_add(1).max(1);
                    let connection_id = inner.next_connection_id;
                    inner.entries.insert(
                        key.clone(),
                        ManagedConnection::Ready {
                            connection_id,
                            backend: Arc::clone(backend),
                            leases: 0,
                            idle_generation: 0,
                            idle_since: None,
                        },
                    );
                }
                waiters
            };
            let wake = result.as_ref().map(|_| ()).map_err(Clone::clone);
            for waiter in waiters {
                let _ = waiter.try_send(wake.clone());
            }
            match result {
                Ok(_) => continue,
                Err(err) => return Err(RemoteAcquireError::Failed(err)),
            }
        }
    }

    fn retain(
        self: &Arc<Self>,
        key: &str,
        expected_connection_id: u64,
    ) -> Option<RemoteConnectionLease> {
        let mut inner = self.inner.lock().unwrap();
        let ManagedConnection::Ready {
            connection_id,
            backend,
            leases,
            idle_generation,
            idle_since,
        } = inner.entries.get_mut(key)?
        else {
            return None;
        };
        if *connection_id != expected_connection_id {
            return None;
        }
        *leases = leases.saturating_add(1);
        *idle_generation = idle_generation.wrapping_add(1);
        *idle_since = None;
        Some(RemoteConnectionLease {
            key: key.to_string(),
            backend: Arc::clone(backend),
            manager: Arc::downgrade(self),
            connection_id: *connection_id,
        })
    }

    fn release(self: &Arc<Self>, key: &str, connection_id: u64) {
        let Some((generation, timeout)) = self.release_state(key, connection_id, Instant::now())
        else {
            return;
        };
        self.schedule_expiry(key.to_string(), connection_id, generation, timeout);
    }

    fn release_state(
        &self,
        key: &str,
        expected_connection_id: u64,
        now: Instant,
    ) -> Option<(u64, Duration)> {
        {
            let mut inner = self.inner.lock().unwrap();
            let timeout = inner.idle_timeout;
            let Some(ManagedConnection::Ready {
                connection_id,
                leases,
                idle_generation,
                idle_since,
                ..
            }) = inner.entries.get_mut(key)
            else {
                return None;
            };
            if *connection_id != expected_connection_id {
                return None;
            }
            *leases = leases.saturating_sub(1);
            if *leases != 0 {
                return None;
            }
            *idle_generation = idle_generation.wrapping_add(1);
            *idle_since = Some(now);
            Some((*idle_generation, timeout))
        }
    }

    fn schedule_expiry(
        self: &Arc<Self>,
        key: String,
        connection_id: u64,
        generation: u64,
        timeout: Duration,
    ) {
        let manager = Arc::clone(self);
        smol::spawn(async move {
            smol::Timer::after(timeout).await;
            manager.expire_if_idle(&key, connection_id, generation, Instant::now());
        })
        .detach();
    }

    fn expire_if_idle(
        &self,
        key: &str,
        expected_connection_id: u64,
        generation: u64,
        now: Instant,
    ) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let timeout = inner.idle_timeout;
        let should_remove = matches!(
            inner.entries.get(key),
            Some(ManagedConnection::Ready {
                connection_id,
                leases: 0,
                idle_generation,
                idle_since: Some(idle_since),
                ..
            }) if *connection_id == expected_connection_id
                && *idle_generation == generation
                && now.saturating_duration_since(*idle_since) >= timeout
        );
        if should_remove {
            inner.entries.remove(key);
        }
        should_remove
    }

    pub(crate) fn set_idle_timeout(self: &Arc<Self>, timeout: Duration) {
        let now = Instant::now();
        let idle_entries = {
            let mut inner = self.inner.lock().unwrap();
            inner.idle_timeout = timeout;
            inner
                .entries
                .iter_mut()
                .filter_map(|(key, entry)| match entry {
                    ManagedConnection::Ready {
                        connection_id,
                        leases: 0,
                        idle_generation,
                        idle_since,
                        ..
                    } => {
                        *idle_generation = idle_generation.wrapping_add(1);
                        let since = *idle_since.get_or_insert(now);
                        let remaining =
                            timeout.saturating_sub(now.saturating_duration_since(since));
                        Some((key.clone(), *connection_id, *idle_generation, remaining))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        for (key, connection_id, generation, remaining) in idle_entries {
            self.schedule_expiry(key, connection_id, generation, remaining);
        }
    }

    #[cfg(test)]
    pub(crate) fn invalidate(&self, key: &str) {
        self.inner.lock().unwrap().entries.remove(key);
    }

    /// Remove `key` only while it still refers to `connection_id`, so a stale
    /// failure report cannot discard a replacement another task already
    /// installed. Deliberately never matches `Connecting`/`Probing`: an
    /// in-flight dial or probe settles on its own verdict, and removing it
    /// here would strand its waiters.
    fn invalidate_connection(&self, key: &str, connection_id: u64) {
        let mut inner = self.inner.lock().unwrap();
        let matches = matches!(
            inner.entries.get(key),
            Some(ManagedConnection::Ready { connection_id: current, .. }) if *current == connection_id
        );
        if matches {
            inner.entries.remove(key);
        }
    }
}

/// Sources the user has explicitly connected at least once in this process.
///
/// Deliberately process-wide, matching [`remote_connection_manager`]: the
/// connection itself is shared between windows, so remembering the click only
/// per-window meant a second window had to ask again for a session that was
/// already open. Never persisted — a restart requires a fresh click, which is
/// what keeps "never dial without being asked" meaningful.
fn authorized_sources() -> &'static Mutex<HashSet<String>> {
    static AUTHORIZED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    AUTHORIZED.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) fn authorize_remote_source(source_key: &str) {
    authorized_sources()
        .lock()
        .unwrap()
        .insert(source_key.to_string());
}

pub(crate) fn remote_source_is_authorized(source_key: &str) -> bool {
    authorized_sources().lock().unwrap().contains(source_key)
}

pub(crate) fn remote_connection_manager() -> Arc<RemoteConnectionManager> {
    static MANAGER: OnceLock<Arc<RemoteConnectionManager>> = OnceLock::new();
    Arc::clone(MANAGER.get_or_init(|| {
        RemoteConnectionManager::new(
            Arc::new(SshRemoteFileConnector),
            Duration::from_secs(crate::native_settings::remote_sftp_idle_minutes() as u64 * 60),
        )
    }))
}

pub(crate) fn update_remote_connection_idle_timeout(minutes: u32) {
    remote_connection_manager().set_idle_timeout(Duration::from_secs(minutes as u64 * 60));
}

/// Drop a cached connection. Used when a connection fails the checks that
/// bring the panel up (resolving the root, listing it): whatever went wrong, a
/// session that cannot complete its own handshake is worthless, and keeping it
/// means every Retry replays the same failure against the same broken session
/// instead of dialling afresh. Guarded by `connection_id` — the failure must
/// only ever remove the connection it actually happened on, never a healthy
/// replacement dialled under the same key since.
pub(crate) fn invalidate_remote_connection(key: &str, connection_id: u64) {
    remote_connection_manager().invalidate_connection(key, connection_id);
}

pub(crate) fn invalidate_remote_connection_if_dead(
    key: &str,
    connection_id: u64,
    message: &str,
) -> bool {
    let lower = message.to_ascii_lowercase();
    // Matches both the wrapper text (`SftpChannelError`) and the innermost
    // cause, because a concise user-facing message carries only the latter.
    // "closed channel" covers async-channel's own wording on both ends:
    // "sending into a closed channel" and "receiving from an empty and closed
    // channel" — the previous "sending on a closed" never matched either.
    let dead = lower.contains("session is dead")
        || lower.contains("channel is closed")
        || lower.contains("channel closed")
        || lower.contains("closed channel")
        || lower.contains("no connection has been set up")
        || lower.contains("connection, but we lost it")
        || lower.contains("failed to send request")
        || lower.contains("failed to receive response")
        || lower.contains("broken pipe");
    if dead {
        remote_connection_manager().invalidate_connection(key, connection_id);
    }
    dead
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemoteFilesPhase {
    Disconnected,
    Connecting,
    Connected,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteDirectory {
    listing: RemoteDirectoryListing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteFileRow {
    pub entry: RemoteFileEntry,
    pub depth: usize,
    pub expanded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemotePreviewStatus {
    None,
    Loading(RemotePath),
    Ready(RemotePath),
    Failed(RemotePath, String),
}

#[derive(Clone, Debug)]
pub(crate) struct RemoteFilesState {
    pub target: Option<RemoteFilesTarget>,
    pub phase: RemoteFilesPhase,
    pub root: Option<RemotePath>,
    directories: HashMap<RemotePath, RemoteDirectory>,
    pub expanded: HashSet<RemotePath>,
    loading_directories: HashSet<RemotePath>,
    pub selected: Option<RemotePath>,
    pub preview: RemotePreviewStatus,
    pub error_message: Option<String>,
    pub generation: u64,
    /// True while a tree restored from cache is on screen but the fresh lease
    /// for it has not arrived yet. Effects that need the lease are deferred
    /// (the intent is already recorded in `loading_directories` / `preview`)
    /// and flushed by the `Connected` event, so a click during that round trip
    /// cannot reach the effect layer, find no lease, and tear the restored
    /// tree down as a spurious connection failure.
    awaiting_lease: bool,
    /// Trees kept across target switches so returning to a workspace does not
    /// re-list a directory the connection could still serve from memory.
    /// Keyed by [`Self::tree_cache_key`] — source *and* requested root —
    /// because two projects can share one host while browsing different
    /// directories, and the wrong tree must never appear under a project's
    /// name.
    cached_trees: HashMap<TreeCacheKey, CachedTree>,
    /// LRU order for `cached_trees`, oldest first.
    cached_tree_order: VecDeque<TreeCacheKey>,
}

/// Identity of a browsed tree: the connection source plus the requested root.
/// The source alone is not enough — see `cached_trees`.
type TreeCacheKey = (String, String);

/// Everything needed to put a previously browsed source straight back on
/// screen. Small by design: entries are `{ path, name, kind, size }` and the
/// row budget caps them, so a handful of these cost far less than one decoded
/// image preview.
#[derive(Clone, Debug)]
struct CachedTree {
    root: RemotePath,
    directories: HashMap<RemotePath, RemoteDirectory>,
    expanded: HashSet<RemotePath>,
    selected: Option<RemotePath>,
}

/// How many sources keep a cached tree. Much smaller than the local panel's
/// `FILE_VIEW_STATE_CACHE_CAP` (32) because each entry here holds up to
/// `REMOTE_FILE_TREE_ROW_LIMIT` rows rather than a scroll offset.
const REMOTE_TREE_CACHE_CAP: usize = 4;

impl Default for RemoteFilesState {
    fn default() -> Self {
        Self {
            target: None,
            phase: RemoteFilesPhase::Disconnected,
            root: None,
            directories: HashMap::new(),
            expanded: HashSet::new(),
            loading_directories: HashSet::new(),
            selected: None,
            preview: RemotePreviewStatus::None,
            error_message: None,
            generation: 0,
            awaiting_lease: false,
            cached_trees: HashMap::new(),
            cached_tree_order: VecDeque::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemoteFilesEffect {
    Connect {
        generation: u64,
        target: RemoteFilesTarget,
        allow_connect: bool,
    },
    ListDirectory {
        generation: u64,
        source_key: String,
        path: RemotePath,
        limit: usize,
    },
    LoadPreview {
        generation: u64,
        source_key: String,
        path: RemotePath,
    },
    ReleaseLease,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemoteFilesEvent {
    TargetChanged(Option<RemoteFilesTarget>),
    ConnectRequested,
    ResumeRequested,
    Connected {
        generation: u64,
        root: RemotePath,
        listing: RemoteDirectoryListing,
    },
    ConnectionFailed {
        generation: u64,
        message: String,
    },
    ResumeUnavailable {
        generation: u64,
    },
    ToggleDirectory(RemotePath),
    DirectoryLoaded {
        generation: u64,
        path: RemotePath,
        listing: RemoteDirectoryListing,
    },
    DirectoryFailed {
        generation: u64,
        path: RemotePath,
        message: String,
    },
    SelectFile(RemotePath),
    PreviewFinished {
        generation: u64,
        path: RemotePath,
        error: Option<String>,
    },
    ClosePreview,
    Refresh,
    PanelHidden,
}

impl RemoteFilesState {
    pub(crate) fn source_key(source: &RemoteFilesSource) -> String {
        match source {
            RemoteFilesSource::SshHost(host) => format!("ssh-host:{host}"),
            RemoteFilesSource::ClientDomain(domain) => format!("client-domain:{domain}"),
        }
    }

    pub(crate) fn current_source_key(&self) -> Option<String> {
        self.target
            .as_ref()
            .map(|target| Self::source_key(&target.source))
    }

    fn tree_cache_key(target: &RemoteFilesTarget) -> TreeCacheKey {
        (
            Self::source_key(&target.source),
            target.requested_root.clone(),
        )
    }

    fn current_tree_cache_key(&self) -> Option<TreeCacheKey> {
        self.target.as_ref().map(Self::tree_cache_key)
    }

    pub(crate) fn can_resume_current(&self) -> bool {
        self.current_source_key()
            .is_some_and(|key| remote_source_is_authorized(&key))
    }

    pub(crate) fn transition(&mut self, event: RemoteFilesEvent) -> Vec<RemoteFilesEffect> {
        match event {
            RemoteFilesEvent::TargetChanged(target) => {
                if self.target == target {
                    return Vec::new();
                }
                // Switching away is not a teardown: keep this source's tree so
                // coming back is instant, and drop only what belongs to the
                // outgoing view.
                self.stash_current_tree();
                self.generation = self.generation.wrapping_add(1);
                self.phase = RemoteFilesPhase::Disconnected;
                self.root = None;
                self.directories.clear();
                self.expanded.clear();
                self.loading_directories.clear();
                self.selected = None;
                self.preview = RemotePreviewStatus::None;
                self.error_message = None;
                self.awaiting_lease = false;
                self.target = target;
                // The lease belongs to the old target; the pool keeps the
                // session warm for whoever asks next.
                let mut effects = vec![RemoteFilesEffect::ReleaseLease];
                if self.restore_cached_tree() {
                    // Already authorized and already browsed: the cached tree
                    // goes on screen immediately while a lease is re-taken and
                    // the root re-listed underneath it.
                    self.awaiting_lease = true;
                    effects.extend(self.refresh_restored_tree());
                }
                effects
            }
            RemoteFilesEvent::ConnectRequested => self.begin_connect(true),
            RemoteFilesEvent::ResumeRequested => self.begin_connect(false),
            RemoteFilesEvent::Connected {
                generation,
                root,
                mut listing,
            } => {
                if generation != self.generation || self.target.is_none() {
                    return vec![RemoteFilesEffect::ReleaseLease];
                }
                if let Some(key) = self.current_source_key() {
                    authorize_remote_source(&key);
                }
                let was_deferred = std::mem::take(&mut self.awaiting_lease);
                self.phase = RemoteFilesPhase::Connected;
                self.error_message = None;
                self.root = Some(root.clone());
                self.expanded.insert(root.clone());
                if listing.entries.len() > REMOTE_FILE_TREE_ROW_LIMIT {
                    listing.entries.truncate(REMOTE_FILE_TREE_ROW_LIMIT);
                    listing.truncated = true;
                }
                // A restored tree may hang off a differently-resolved root
                // (e.g. the remote home moved); anything not under the fresh
                // root can never render, so it must not eat the row budget.
                self.directories
                    .retain(|path, _| *path == root || path.is_descendant_of(&root));
                self.expanded
                    .retain(|path| *path == root || path.is_descendant_of(&root));
                self.loading_directories
                    .retain(|path| *path == root || path.is_descendant_of(&root));
                self.directories.insert(root, RemoteDirectory { listing });
                self.enforce_row_budget();
                if !was_deferred {
                    return Vec::new();
                }
                // Flush the operations recorded while the restored tree waited
                // for its lease. `loading_directories` and a `Loading` preview
                // are the queue: they captured the user's intent, and the
                // lease that just arrived is what they were waiting on.
                let mut effects = Vec::new();
                let pending: Vec<RemotePath> = self.loading_directories.iter().cloned().collect();
                for path in pending {
                    effects.extend(self.list_directory_effect(path));
                }
                if let RemotePreviewStatus::Loading(path) = &self.preview {
                    if let Some(source_key) = self.current_source_key() {
                        effects.push(RemoteFilesEffect::LoadPreview {
                            generation: self.generation,
                            source_key,
                            path: path.clone(),
                        });
                    }
                }
                effects
            }
            RemoteFilesEvent::ConnectionFailed {
                generation,
                message,
            } => {
                if generation == self.generation {
                    // A failed source must not leave a cached tree behind:
                    // switching away and back would restore rows belonging to a
                    // connection that is already gone.
                    self.discard_cached_tree_for_current_target();
                    self.awaiting_lease = false;
                    self.phase = RemoteFilesPhase::Failed(message);
                    self.error_message = None;
                    self.root = None;
                    self.directories.clear();
                    self.expanded.clear();
                    self.loading_directories.clear();
                    self.selected = None;
                    self.preview = RemotePreviewStatus::None;
                }
                Vec::new()
            }
            RemoteFilesEvent::ResumeUnavailable { generation } => {
                if generation == self.generation {
                    self.awaiting_lease = false;
                    self.phase = RemoteFilesPhase::Disconnected;
                    self.error_message = None;
                }
                Vec::new()
            }
            RemoteFilesEvent::ToggleDirectory(path) => {
                if !matches!(self.phase, RemoteFilesPhase::Connected) {
                    return Vec::new();
                }
                if self.expanded.remove(&path) {
                    self.remove_descendants(&path);
                    return Vec::new();
                }
                self.expanded.insert(path.clone());
                if self.directories.contains_key(&path)
                    || !self.loading_directories.insert(path.clone())
                {
                    return Vec::new();
                }
                if self.awaiting_lease {
                    // Recorded in `loading_directories`; `Connected` flushes it
                    // once the lease for the restored tree lands.
                    return Vec::new();
                }
                self.list_directory_effect(path)
            }
            RemoteFilesEvent::DirectoryLoaded {
                generation,
                path,
                mut listing,
            } => {
                if generation != self.generation || !self.expanded.contains(&path) {
                    return Vec::new();
                }
                self.loading_directories.remove(&path);
                self.error_message = None;
                let retained_without_current = self.total_entries().saturating_sub(
                    self.directories
                        .get(&path)
                        .map_or(0, |directory| directory.listing.entries.len()),
                );
                let remaining = REMOTE_FILE_TREE_ROW_LIMIT.saturating_sub(retained_without_current);
                if listing.entries.len() > remaining {
                    listing.entries.truncate(remaining);
                    listing.truncated = true;
                }
                self.directories.insert(path, RemoteDirectory { listing });
                Vec::new()
            }
            RemoteFilesEvent::DirectoryFailed {
                generation,
                path,
                message,
            } => {
                if generation == self.generation {
                    self.loading_directories.remove(&path);
                    self.error_message = Some(message);
                }
                Vec::new()
            }
            RemoteFilesEvent::SelectFile(path) => {
                self.selected = Some(path.clone());
                self.preview = RemotePreviewStatus::Loading(path.clone());
                if self.awaiting_lease {
                    // The `Loading` preview is the queue entry; `Connected`
                    // flushes it once the lease lands.
                    return Vec::new();
                }
                let Some(source_key) = self.current_source_key() else {
                    return Vec::new();
                };
                vec![RemoteFilesEffect::LoadPreview {
                    generation: self.generation,
                    source_key,
                    path,
                }]
            }
            RemoteFilesEvent::PreviewFinished {
                generation,
                path,
                error,
            } => {
                if generation != self.generation || self.selected.as_ref() != Some(&path) {
                    return Vec::new();
                }
                self.preview = match error {
                    Some(error) => RemotePreviewStatus::Failed(path, error),
                    None => RemotePreviewStatus::Ready(path),
                };
                Vec::new()
            }
            RemoteFilesEvent::ClosePreview => {
                self.selected = None;
                self.preview = RemotePreviewStatus::None;
                Vec::new()
            }
            RemoteFilesEvent::Refresh => {
                if self.awaiting_lease {
                    // A refresh is already in flight: that is literally what
                    // the restored tree is waiting on. Bumping the generation
                    // here would orphan it and leave the panel stuck.
                    return Vec::new();
                }
                let Some(root) = self.root.clone() else {
                    return Vec::new();
                };
                self.generation = self.generation.wrapping_add(1);
                self.directories.clear();
                self.error_message = None;
                self.expanded.clear();
                self.expanded.insert(root.clone());
                self.loading_directories.clear();
                self.loading_directories.insert(root.clone());
                self.selected = None;
                self.preview = RemotePreviewStatus::None;
                self.list_directory_effect(root)
            }
            RemoteFilesEvent::PanelHidden => {
                // Hiding keeps the structure (it is small) and gives back the
                // lease plus the preview, which is where the memory actually
                // is. `release_cached_trees` frees the rest if the panel stays
                // hidden long enough.
                self.stash_current_tree();
                self.generation = self.generation.wrapping_add(1);
                self.phase = RemoteFilesPhase::Disconnected;
                self.root = None;
                self.directories.clear();
                self.expanded.clear();
                self.loading_directories.clear();
                self.selected = None;
                self.preview = RemotePreviewStatus::None;
                self.error_message = None;
                self.awaiting_lease = false;
                vec![RemoteFilesEffect::ReleaseLease]
            }
        }
    }

    fn begin_connect(&mut self, allow_connect: bool) -> Vec<RemoteFilesEffect> {
        let Some(target) = self.target.clone() else {
            return Vec::new();
        };
        let source_key = Self::source_key(&target.source);
        let authorized = remote_source_is_authorized(&source_key);
        if !allow_connect && !authorized {
            // Never dial a source the user has not asked for.
            return Vec::new();
        }
        if matches!(
            self.phase,
            RemoteFilesPhase::Connecting | RemoteFilesPhase::Connected
        ) {
            return Vec::new();
        }
        self.generation = self.generation.wrapping_add(1);
        self.phase = RemoteFilesPhase::Connecting;
        vec![RemoteFilesEffect::Connect {
            generation: self.generation,
            target,
            // An already-authorized source may redial: once the pool's 15
            // minute idle window lapses, silently reconnecting is better than
            // dropping the user back onto a Connect button they already
            // pressed for this host.
            allow_connect: allow_connect || authorized,
        }]
    }

    /// Re-acquire a lease and refresh the root for a tree that was restored
    /// from cache. Unlike [`Self::begin_connect`] this runs while the phase is
    /// already `Connected`: the user keeps looking at the cached listing
    /// instead of a spinner, and `Connected` swaps in the fresh root when it
    /// lands. A failure here is real (the session is gone) and falls through to
    /// the normal failed state.
    fn refresh_restored_tree(&mut self) -> Vec<RemoteFilesEffect> {
        let Some(target) = self.target.clone() else {
            return Vec::new();
        };
        self.generation = self.generation.wrapping_add(1);
        vec![RemoteFilesEffect::Connect {
            generation: self.generation,
            target,
            allow_connect: true,
        }]
    }

    fn list_directory_effect(&self, path: RemotePath) -> Vec<RemoteFilesEffect> {
        let Some(source_key) = self.current_source_key() else {
            return Vec::new();
        };
        let remaining = REMOTE_FILE_TREE_ROW_LIMIT.saturating_sub(self.total_entries());
        vec![RemoteFilesEffect::ListDirectory {
            generation: self.generation,
            source_key,
            path,
            limit: remaining,
        }]
    }

    fn total_entries(&self) -> usize {
        self.directories
            .values()
            .map(|directory| directory.listing.entries.len())
            .sum()
    }

    fn remove_descendants(&mut self, path: &RemotePath) {
        self.expanded
            .retain(|candidate| !candidate.is_descendant_of(path));
        self.loading_directories
            .retain(|candidate| candidate != path && !candidate.is_descendant_of(path));
        self.directories
            .retain(|candidate, _| candidate != path && !candidate.is_descendant_of(path));
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| selected.is_descendant_of(path))
        {
            self.selected = None;
            self.preview = RemotePreviewStatus::None;
        }
    }

    /// Forget any cached tree for the target currently in view. Used when that
    /// target is known bad, so the cache cannot resurrect it later.
    fn discard_cached_tree_for_current_target(&mut self) {
        let Some(cache_key) = self.current_tree_cache_key() else {
            return;
        };
        self.cached_trees.remove(&cache_key);
        if let Some(index) = self
            .cached_tree_order
            .iter()
            .position(|key| *key == cache_key)
        {
            self.cached_tree_order.remove(index);
        }
    }

    /// Move the loaded tree into the cache instead of dropping it, so switching
    /// back to this target can show it again without re-listing.
    fn stash_current_tree(&mut self) {
        let (Some(cache_key), Some(root)) = (self.current_tree_cache_key(), self.root.clone())
        else {
            return;
        };
        if self.directories.is_empty() {
            return;
        }
        let tree = CachedTree {
            root,
            directories: std::mem::take(&mut self.directories),
            expanded: std::mem::take(&mut self.expanded),
            selected: self.selected.take(),
        };
        if self.cached_trees.insert(cache_key.clone(), tree).is_none() {
            self.cached_tree_order.push_back(cache_key);
        } else {
            self.touch_cached_tree(&cache_key);
        }
        while self.cached_tree_order.len() > REMOTE_TREE_CACHE_CAP {
            if let Some(evicted) = self.cached_tree_order.pop_front() {
                self.cached_trees.remove(&evicted);
            }
        }
    }

    fn touch_cached_tree(&mut self, cache_key: &TreeCacheKey) {
        if let Some(index) = self
            .cached_tree_order
            .iter()
            .position(|key| key == cache_key)
        {
            self.cached_tree_order.remove(index);
        }
        self.cached_tree_order.push_back(cache_key.clone());
    }

    /// Put a cached tree back on screen. Only legitimate for a source the user
    /// already authorized — the connection pool's liveness probe makes reusing
    /// the underlying session safe, and a stale listing is corrected by the
    /// next expand.
    fn restore_cached_tree(&mut self) -> bool {
        let Some(cache_key) = self.current_tree_cache_key() else {
            return false;
        };
        if !remote_source_is_authorized(&cache_key.0) {
            return false;
        }
        let Some(tree) = self.cached_trees.remove(&cache_key) else {
            return false;
        };
        if let Some(index) = self
            .cached_tree_order
            .iter()
            .position(|key| *key == cache_key)
        {
            self.cached_tree_order.remove(index);
        }
        self.root = Some(tree.root);
        self.directories = tree.directories;
        self.expanded = tree.expanded;
        self.selected = tree.selected;
        self.phase = RemoteFilesPhase::Connected;
        self.error_message = None;
        true
    }

    /// Trim the tree back under the global row budget. A restored cache can
    /// hold up to the full budget on its own, and the refreshed root listing
    /// arrives budgeted only against itself, so their union can exceed the
    /// bound. Deepest directories collapse first, keeping the shallow
    /// structure the user is most likely looking at.
    fn enforce_row_budget(&mut self) {
        let Some(root) = self.root.clone() else {
            return;
        };
        while self.total_entries() > REMOTE_FILE_TREE_ROW_LIMIT {
            let Some(victim) = self
                .directories
                .keys()
                .filter(|path| **path != root)
                .max_by_key(|path| path.as_str().matches('/').count())
                .cloned()
            else {
                break;
            };
            self.expanded.remove(&victim);
            self.remove_descendants(&victim);
        }
    }

    /// Forget the selected row and any preview tied to it. Mirrors what
    /// closing the preview does to the local panel's `file_selected`, and is
    /// what stops `right_sidebar_file_preview_active` from keeping the pane
    /// open (and the sidebar wide) after the preview is gone.
    pub(crate) fn clear_selection(&mut self) {
        self.selected = None;
        self.preview = RemotePreviewStatus::None;
    }

    /// Drop every cached tree. Wired into the sidebar's existing idle-release
    /// path so a panel left hidden eventually gives the memory back.
    pub(crate) fn release_cached_trees(&mut self) {
        self.cached_trees.clear();
        self.cached_tree_order.clear();
    }

    pub(crate) fn rows(&self) -> Vec<RemoteFileRow> {
        let Some(root) = self.root.as_ref() else {
            return Vec::new();
        };
        let root_name = self
            .target
            .as_ref()
            .map(|target| target.project_name.clone())
            .unwrap_or_else(|| root.file_name().to_string());
        let mut rows = vec![RemoteFileRow {
            entry: RemoteFileEntry {
                path: root.clone(),
                name: root_name,
                kind: RemoteFileKind::Directory,
                size: None,
            },
            depth: 0,
            expanded: self.expanded.contains(root),
        }];
        self.append_rows(root, 1, &mut rows);
        rows
    }

    fn append_rows(&self, parent: &RemotePath, depth: usize, rows: &mut Vec<RemoteFileRow>) {
        if !self.expanded.contains(parent) {
            return;
        }
        let Some(directory) = self.directories.get(parent) else {
            return;
        };
        for entry in &directory.listing.entries {
            let expanded = entry.is_directory() && self.expanded.contains(&entry.path);
            rows.push(RemoteFileRow {
                entry: entry.clone(),
                depth,
                expanded,
            });
            if entry.is_directory() {
                self.append_rows(&entry.path, depth.saturating_add(1), rows);
            }
        }
    }

    pub(crate) fn has_truncated_directory(&self) -> bool {
        self.directories
            .values()
            .any(|directory| directory.listing.truncated)
    }

    pub(crate) fn kind_for_path(&self, path: &RemotePath) -> Option<RemoteFileKind> {
        if self.root.as_ref() == Some(path) {
            return Some(RemoteFileKind::Directory);
        }
        self.directories
            .values()
            .flat_map(|directory| directory.listing.entries.iter())
            .find(|entry| &entry.path == path)
            .map(|entry| entry.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Records how many times the remote home was asked for, so a test can
    /// prove an absolute root never pays for that round-trip.
    #[derive(Default)]
    struct HomeProbeCounter {
        calls: AtomicUsize,
        home: Option<&'static str>,
    }

    impl HomeProbeCounter {
        /// Mirrors `SftpRemoteFileBackend::resolve_root` without a server: the
        /// closure stands in for `sftp.canonicalize`.
        fn resolve(&self, requested: &str) -> Result<RemotePath, String> {
            if !requested_root_needs_home(requested) {
                return remote_root_from_absolute(requested);
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            let home = self
                .home
                .ok_or_else(|| "Can't read the remote home directory: denied".to_string())?;
            resolve_requested_root(RemotePath::from_server_absolute(home)?, requested)
        }
    }

    fn with_home() -> HomeProbeCounter {
        HomeProbeCounter {
            calls: AtomicUsize::new(0),
            home: Some("/home/ada"),
        }
    }

    #[test]
    fn absolute_root_never_resolves_the_remote_home() {
        // The bug this pins: `resolve_root` used to canonicalize `.` first, so a
        // project with a fully specified path still broke on any server that
        // could not answer that request.
        let probe = with_home();
        assert_eq!(probe.resolve("/srv/app").unwrap().as_str(), "/srv/app");
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0);

        // Even with no home available at all, an absolute root still works.
        let no_home = HomeProbeCounter::default();
        assert_eq!(no_home.resolve("/srv/app").unwrap().as_str(), "/srv/app");
        assert_eq!(no_home.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn tilde_resolves_to_the_remote_home() {
        let probe = with_home();
        assert_eq!(probe.resolve("~").unwrap().as_str(), "/home/ada");
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn tilde_child_joins_beneath_the_home() {
        let probe = with_home();
        assert_eq!(
            probe.resolve("~/code/thinkterm").unwrap().as_str(),
            "/home/ada/code/thinkterm"
        );
        // Redundant separators and `.` segments collapse rather than escaping.
        assert_eq!(
            probe.resolve("~/code//./thinkterm").unwrap().as_str(),
            "/home/ada/code/thinkterm"
        );
    }

    #[test]
    fn traversal_is_rejected_on_both_paths() {
        let probe = with_home();
        // `~/..` goes through join_name, which refuses `..`.
        assert!(probe.resolve("~/../etc").is_err());
        // An absolute root is checked before it can ever reach the server.
        let err = probe.resolve("/srv/../etc").unwrap_err();
        assert!(err.contains(".."), "{}", err);
    }

    #[test]
    fn relative_roots_are_rejected_with_a_clear_message() {
        let probe = with_home();
        for bad in ["relative/path", "", "C:/windows"] {
            let err = probe.resolve(bad).unwrap_err();
            assert!(
                err.contains("absolute path or ~"),
                "{:?} produced {:?}",
                bad,
                err
            );
        }
        // The failure is decided locally; no home lookup was attempted.
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn remote_paths_stay_slash_only() {
        // Guards the Windows client: joining must never introduce a `\`, and a
        // backslash is an ordinary character in a POSIX name.
        let root = RemotePath::from_server_absolute("/srv").unwrap();
        let child = root.join_name("a b").unwrap();
        assert_eq!(child.as_str(), "/srv/a b");
        assert_eq!(root.join_name("we\\ird").unwrap().as_str(), "/srv/we\\ird");
        assert!(root.join_name("nested/name").is_err());
        assert!(root.join_name("..").is_err());
        // A trailing slash from the server is normalized away, but root stays "/".
        assert_eq!(
            RemotePath::from_server_absolute("/srv/app/")
                .unwrap()
                .as_str(),
            "/srv/app"
        );
        assert_eq!(RemotePath::from_server_absolute("/").unwrap().as_str(), "/");
    }

    #[test]
    fn an_idle_connection_that_died_is_replaced_without_the_user_retrying() {
        // The reported symptom: the panel occasionally fails with "can't read
        // the remote home" and works on a second attempt. Reusing a cached
        // connection that the server reaped while the panel was closed must
        // redial transparently instead of surfacing that failure.
        let calls = Arc::new(AtomicUsize::new(0));
        let probes = Arc::new(AtomicUsize::new(0));
        let manager = RemoteConnectionManager::new(
            Arc::new(DyingConnector {
                calls: Arc::clone(&calls),
                probes: Arc::clone(&probes),
                first_is_alive: Arc::new(AtomicBool::new(true)),
            }),
            Duration::from_secs(300),
        );
        let key = "host".to_string();

        let lease = smol::block_on(manager.acquire(key.clone(), SshDomain::default(), true))
            .expect("first acquire");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // A fresh connection is not probed; it was just built.
        assert_eq!(probes.load(Ordering::SeqCst), 0);

        // Panel closed: the connection goes idle and (unknown to us) dies.
        drop(lease);

        // Panel reopened. The cached entry is probed, found dead, and replaced
        // — the caller still gets a usable lease on the first try.
        let lease = smol::block_on(manager.acquire(key.clone(), SshDomain::default(), true))
            .expect("acquire must transparently redial");
        assert_eq!(
            probes.load(Ordering::SeqCst),
            1,
            "cached entry must be probed"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2, "must have redialled");

        // Still leased, so the next acquire skips the probe entirely.
        let second = smol::block_on(manager.acquire(key.clone(), SshDomain::default(), true))
            .expect("concurrent acquire");
        assert_eq!(
            probes.load(Ordering::SeqCst),
            1,
            "live lease must not be probed"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        drop(second);
        drop(lease);
    }

    /// Backend whose probe blocks until the test opens the gate, so the test
    /// can observe what other acquires do while a probe is in flight.
    struct GatedProbeBackend {
        probes: Arc<AtomicUsize>,
        gate: smol::channel::Receiver<()>,
    }

    impl RemoteFileBackend for GatedProbeBackend {
        fn resolve_root(&self, _requested: String) -> RemoteFuture<RemotePath> {
            Box::pin(async { RemotePath::from_server_absolute("/home/test") })
        }

        fn list_directory(
            &self,
            _path: RemotePath,
            _limit: usize,
        ) -> RemoteFuture<RemoteDirectoryListing> {
            Box::pin(async {
                Ok(RemoteDirectoryListing {
                    entries: Vec::new(),
                    truncated: false,
                })
            })
        }

        fn read_file(&self, _path: RemotePath, _limit: usize) -> RemoteFuture<RemoteFileBytes> {
            Box::pin(async {
                Ok(RemoteFileBytes {
                    bytes: Vec::new(),
                    truncated: false,
                })
            })
        }

        fn probe(&self) -> RemoteFuture<()> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            let gate = self.gate.clone();
            Box::pin(async move {
                let _ = gate.recv().await;
                Ok(())
            })
        }
    }

    struct GatedProbeConnector {
        probes: Arc<AtomicUsize>,
        gate: smol::channel::Receiver<()>,
    }

    impl RemoteFileConnector for GatedProbeConnector {
        fn connect(&self, _config: SshDomain) -> RemoteFuture<Arc<dyn RemoteFileBackend>> {
            let backend = GatedProbeBackend {
                probes: Arc::clone(&self.probes),
                gate: self.gate.clone(),
            };
            Box::pin(async move { Ok(Arc::new(backend) as Arc<dyn RemoteFileBackend>) })
        }
    }

    /// While one acquire verifies an idle connection, a concurrent acquire
    /// must wait for the verdict. Handing out the unverified backend meant a
    /// caller could run operations on a session about to be declared dead —
    /// and its failure report could then tear down the healthy replacement.
    #[test]
    fn acquires_during_a_liveness_probe_wait_for_its_verdict() {
        let probes = Arc::new(AtomicUsize::new(0));
        let (open_gate, gate) = smol::channel::unbounded::<()>();
        let manager = RemoteConnectionManager::new(
            Arc::new(GatedProbeConnector {
                probes: Arc::clone(&probes),
                gate,
            }),
            Duration::from_secs(300),
        );
        let key = "host".to_string();

        smol::block_on(async {
            let lease = manager
                .acquire(key.clone(), SshDomain::default(), true)
                .await
                .expect("first acquire");
            drop(lease); // idle: the next acquire must probe

            let prober = {
                let manager = Arc::clone(&manager);
                let key = key.clone();
                smol::spawn(async move { manager.acquire(key, SshDomain::default(), true).await })
            };
            while probes.load(Ordering::SeqCst) == 0 {
                smol::Timer::after(Duration::from_millis(1)).await;
            }

            let (done_tx, done_rx) = smol::channel::bounded::<()>(1);
            let waiter = {
                let manager = Arc::clone(&manager);
                let key = key.clone();
                smol::spawn(async move {
                    let lease = manager.acquire(key, SshDomain::default(), true).await;
                    let _ = done_tx.send(()).await;
                    lease
                })
            };
            smol::Timer::after(Duration::from_millis(50)).await;
            assert!(
                done_rx.try_recv().is_err(),
                "the second acquire must wait for the probe verdict"
            );
            assert_eq!(
                probes.load(Ordering::SeqCst),
                1,
                "the waiter must not start a second probe"
            );

            open_gate.send(()).await.expect("open the probe gate");
            let first = prober.await.expect("probing acquire");
            let second = waiter.await.expect("waiting acquire");
            assert_eq!(
                probes.load(Ordering::SeqCst),
                1,
                "one probe verdict serves every waiter"
            );
            drop(first);
            drop(second);
        });
    }

    /// The liveness probe accepts the same home spellings as root resolution;
    /// a server that only answers the empty path is alive, and treating it as
    /// dead forced a redial on every idle reacquire.
    #[test]
    fn the_home_probe_falls_back_to_the_empty_spelling() {
        let calls = std::cell::RefCell::new(Vec::new());
        let result = smol::block_on(canonicalize_remote_home(|spelling| {
            calls.borrow_mut().push(spelling);
            async move {
                if spelling.is_empty() {
                    Ok("/home/ada".to_string())
                } else {
                    Err("Library-specific error")
                }
            }
        }));
        assert_eq!(result.expect("fallback must succeed"), "/home/ada");
        assert_eq!(*calls.borrow(), vec![".", ""]);

        let result: Result<String, Vec<&str>> =
            smol::block_on(canonicalize_remote_home(|_| async { Err("denied") }));
        assert_eq!(
            result.expect_err("every spelling failed"),
            vec!["denied", "denied"],
            "failures must be reported in spelling order"
        );
    }

    #[test]
    fn a_failed_startup_drops_the_cached_connection_so_retry_redials() {
        // The reported symptom: after one good session, reconnecting fails and
        // Retry keeps failing. Whatever broke the session, the panel must not
        // hand the same one back on the next attempt.
        let calls = Arc::new(AtomicUsize::new(0));
        let manager = RemoteConnectionManager::new(
            Arc::new(FakeConnector {
                calls: Arc::clone(&calls),
            }),
            Duration::from_secs(30),
        );
        let key = "host".to_string();

        let lease = smol::block_on(manager.acquire(key.clone(), SshDomain::default(), true))
            .expect("first acquire");
        drop(lease);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Still cached: a plain reconnect reuses the session without dialling.
        let reused = smol::block_on(manager.acquire(key.clone(), SshDomain::default(), true))
            .expect("cached acquire");
        drop(reused);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Startup failed on that cached session, so it is thrown away and the
        // next attempt dials a fresh one instead of replaying the failure.
        manager.invalidate(&key);
        let fresh = smol::block_on(manager.acquire(key.clone(), SshDomain::default(), true))
            .expect("acquire after invalidation");
        drop(fresh);
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // And a resume (which must never dial on its own) correctly reports
        // that there is nothing to resume once the entry is gone.
        manager.invalidate(&key);
        assert!(matches!(
            smol::block_on(manager.acquire(key, SshDomain::default(), false)),
            Err(RemoteAcquireError::NotConnected)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_dead_transport_is_detected_from_the_concise_message_too() {
        // `resolve_root` hands the UI only the innermost cause, so invalidation
        // must recognise the transport failure from that alone — otherwise
        // Retry keeps handing back the same dead cached connection.
        for message in [
            "sending into a closed channel",
            "receiving from an empty and closed channel",
            "Failed to send request: sending into a closed channel",
            "session is dead",
            "Broken pipe",
        ] {
            assert!(
                invalidate_remote_connection_if_dead("unused-key", 0, message),
                "{:?} should invalidate",
                message
            );
        }
        // A server-side refusal is not a dead connection: keep the session.
        for message in ["Permission denied", "No such file or directory"] {
            assert!(
                !invalidate_remote_connection_if_dead("unused-key", 0, message),
                "{:?} should not invalidate",
                message
            );
        }
    }

    #[test]
    fn error_chain_keeps_the_specific_cause() {
        // "Library-specific error" alone is what made the UI message useless.
        #[derive(Debug)]
        struct Inner;
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "Permission denied")
            }
        }
        impl std::error::Error for Inner {}

        #[derive(Debug)]
        struct Outer(Inner);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "Library-specific error")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let err = Outer(Inner);
        assert_eq!(
            error_chain(&err),
            "Library-specific error: Permission denied"
        );
        assert_eq!(error_summary(&err), "Permission denied");
    }

    /// Authorization is a process-wide registry, so tests sharing a source name
    /// would inherit each other's authorization (and race, since they run in
    /// parallel). Every test that cares about the authorized/unauthorized
    /// distinction must mint its own name through this.
    fn unique_source(prefix: &str) -> String {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        format!("{prefix}-{}", NEXT.fetch_add(1, Ordering::SeqCst))
    }

    fn target(source: &str) -> RemoteFilesTarget {
        target_with_root(source, "~")
    }

    fn target_with_root(source: &str, requested_root: &str) -> RemoteFilesTarget {
        RemoteFilesTarget {
            project_id: "project".to_string(),
            project_name: "Project".to_string(),
            source: RemoteFilesSource::ClientDomain(source.to_string()),
            requested_root: requested_root.to_string(),
        }
    }

    fn listing(parent: &RemotePath, names: &[(&str, RemoteFileKind)]) -> RemoteDirectoryListing {
        RemoteDirectoryListing {
            entries: names
                .iter()
                .map(|(name, kind)| RemoteFileEntry {
                    path: parent.join_name(name).unwrap(),
                    name: (*name).to_string(),
                    kind: *kind,
                    size: None,
                })
                .collect(),
            truncated: false,
        }
    }

    struct FakeBackend;

    impl RemoteFileBackend for FakeBackend {
        fn resolve_root(&self, _requested: String) -> RemoteFuture<RemotePath> {
            Box::pin(async { RemotePath::from_server_absolute("/home/test") })
        }

        fn list_directory(
            &self,
            _path: RemotePath,
            _limit: usize,
        ) -> RemoteFuture<RemoteDirectoryListing> {
            Box::pin(async {
                Ok(RemoteDirectoryListing {
                    entries: Vec::new(),
                    truncated: false,
                })
            })
        }

        fn read_file(&self, _path: RemotePath, _limit: usize) -> RemoteFuture<RemoteFileBytes> {
            Box::pin(async {
                Ok(RemoteFileBytes {
                    bytes: Vec::new(),
                    truncated: false,
                })
            })
        }
    }

    struct FakeConnector {
        calls: Arc<AtomicUsize>,
    }

    /// Backend that answers once and then behaves like a session the server
    /// reaped while it sat idle: every later probe fails.
    struct DiesWhenIdleBackend {
        probes: Arc<AtomicUsize>,
        alive: Arc<AtomicBool>,
    }

    impl RemoteFileBackend for DiesWhenIdleBackend {
        fn resolve_root(&self, _requested: String) -> RemoteFuture<RemotePath> {
            Box::pin(async { RemotePath::from_server_absolute("/home/test") })
        }

        fn list_directory(
            &self,
            _path: RemotePath,
            _limit: usize,
        ) -> RemoteFuture<RemoteDirectoryListing> {
            Box::pin(async {
                Ok(RemoteDirectoryListing {
                    entries: Vec::new(),
                    truncated: false,
                })
            })
        }

        fn read_file(&self, _path: RemotePath, _limit: usize) -> RemoteFuture<RemoteFileBytes> {
            Box::pin(async {
                Ok(RemoteFileBytes {
                    bytes: Vec::new(),
                    truncated: false,
                })
            })
        }

        fn probe(&self) -> RemoteFuture<()> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            let alive = self.alive.load(Ordering::SeqCst);
            Box::pin(async move {
                if alive {
                    Ok(())
                } else {
                    Err("Library-specific error: session is gone".to_string())
                }
            })
        }
    }

    struct DyingConnector {
        calls: Arc<AtomicUsize>,
        probes: Arc<AtomicUsize>,
        /// Flipped false after the first connection is created, so the *next*
        /// dial produces a healthy backend while the cached one is dead.
        first_is_alive: Arc<AtomicBool>,
    }

    impl RemoteFileConnector for DyingConnector {
        fn connect(&self, _config: SshDomain) -> RemoteFuture<Arc<dyn RemoteFileBackend>> {
            let nth = self.calls.fetch_add(1, Ordering::SeqCst);
            let alive = Arc::new(AtomicBool::new(nth > 0));
            if nth == 0 {
                self.first_is_alive.store(false, Ordering::SeqCst);
            }
            let backend = DiesWhenIdleBackend {
                probes: Arc::clone(&self.probes),
                alive,
            };
            Box::pin(async move { Ok(Arc::new(backend) as Arc<dyn RemoteFileBackend>) })
        }
    }

    impl RemoteFileConnector for FakeConnector {
        fn connect(&self, _config: SshDomain) -> RemoteFuture<Arc<dyn RemoteFileBackend>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(Arc::new(FakeBackend) as Arc<dyn RemoteFileBackend>) })
        }
    }

    #[test]
    fn remote_path_is_slash_only_and_treats_backslash_as_a_filename_character() {
        let root = RemotePath::from_server_absolute("/root/app/").unwrap();
        assert_eq!(root.as_str(), "/root/app");
        assert_eq!(
            RemotePath::from_server_absolute("/root/app ")
                .unwrap()
                .as_str(),
            "/root/app "
        );
        assert_eq!(root.join_name("src").unwrap().as_str(), "/root/app/src");
        assert_eq!(
            root.join_name(r"odd\\name").unwrap().as_str(),
            r"/root/app/odd\\name"
        );
        assert!(root.join_name("../escape").is_err());
        assert!(root.join_name("a/b").is_err());
    }

    #[test]
    fn tilde_is_resolved_from_canonical_dot_home() {
        let home = RemotePath::from_server_absolute("/home/alice").unwrap();
        assert_eq!(resolve_requested_root(home.clone(), "~").unwrap(), home);
        assert_eq!(
            resolve_requested_root(home, "~/src/app").unwrap().as_str(),
            "/home/alice/src/app"
        );
        assert!(resolve_requested_root(
            RemotePath::from_server_absolute("/home/alice").unwrap(),
            "~/../etc"
        )
        .is_err());
    }

    #[test]
    fn raw_directory_reader_only_treats_real_eof_as_completion() {
        let protocol_eof = anyhow::Error::new(SftpChannelError::Sftp(SftpError::Eof));
        assert!(is_sftp_directory_eof(&protocol_eof));
        let io_eof = anyhow::Error::new(SftpChannelError::FileIo(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "no more files",
        )));
        assert!(is_sftp_directory_eof(&io_eof));
        let lost = anyhow::Error::new(SftpChannelError::Sftp(SftpError::ConnectionLost));
        assert!(!is_sftp_directory_eof(&lost));
    }

    #[test]
    fn manager_single_flights_and_expires_only_after_last_lease() {
        let calls = Arc::new(AtomicUsize::new(0));
        let manager = RemoteConnectionManager::new(
            Arc::new(FakeConnector {
                calls: Arc::clone(&calls),
            }),
            Duration::from_secs(30),
        );
        let (first, second) = smol::block_on(async {
            futures::future::join(
                manager.acquire("source".to_string(), SshDomain::default(), true),
                manager.acquire("source".to_string(), SshDomain::default(), true),
            )
            .await
        });
        let mut first = first.unwrap();
        let mut second = second.unwrap();
        let connection_id = first.connection_id;
        assert_eq!(second.connection_id, connection_id);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        first.manager = Weak::new();
        second.manager = Weak::new();
        let idle_since = Instant::now();
        assert!(manager
            .release_state("source", connection_id, idle_since)
            .is_none());
        drop(first);
        {
            let inner = manager.inner.lock().unwrap();
            assert!(matches!(
                inner.entries.get("source"),
                Some(ManagedConnection::Ready { leases: 1, .. })
            ));
        }
        let (generation, _) = manager
            .release_state("source", connection_id, idle_since)
            .expect("last lease starts idle timer");
        drop(second);
        let stored_idle_since = {
            let inner = manager.inner.lock().unwrap();
            match inner.entries.get("source") {
                Some(ManagedConnection::Ready {
                    leases: 0,
                    idle_generation,
                    idle_since: Some(idle_since),
                    ..
                }) => {
                    assert_eq!(*idle_generation, generation);
                    *idle_since
                }
                _ => panic!("expected idle connection"),
            }
        };
        assert!(!manager.expire_if_idle(
            "source",
            connection_id,
            generation,
            stored_idle_since + Duration::from_secs(29)
        ));
        assert!(manager.expire_if_idle(
            "source",
            connection_id,
            generation,
            stored_idle_since + Duration::from_secs(30)
        ));
    }

    #[test]
    fn stale_lease_cannot_release_a_replacement_connection() {
        let calls = Arc::new(AtomicUsize::new(0));
        let manager = RemoteConnectionManager::new(
            Arc::new(FakeConnector {
                calls: Arc::clone(&calls),
            }),
            Duration::from_secs(30),
        );
        let old = smol::block_on(manager.acquire("source".to_string(), SshDomain::default(), true))
            .unwrap();
        let old_id = old.connection_id;
        manager.invalidate("source");
        let replacement =
            smol::block_on(manager.acquire("source".to_string(), SshDomain::default(), true))
                .unwrap();
        assert_ne!(old_id, replacement.connection_id);
        drop(old);
        let inner = manager.inner.lock().unwrap();
        assert!(matches!(
            inner.entries.get("source"),
            Some(ManagedConnection::Ready { leases: 1, .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn connection_key_changes_with_endpoint_options_and_credentials() {
        let mut first = SshDomain::default();
        first.remote_address = "host:22".to_string();
        first.username = Some("alice".to_string());
        let base = remote_connection_key("source", &first);

        let mut changed_host = first.clone();
        changed_host.remote_address = "other:22".to_string();
        assert_ne!(base, remote_connection_key("source", &changed_host));

        let mut changed_option = first.clone();
        changed_option
            .ssh_option
            .insert("proxycommand".to_string(), "ssh jump".to_string());
        assert_ne!(base, remote_connection_key("source", &changed_option));

        let mut changed_password = first;
        changed_password.stored_password = Some("secret".to_string());
        assert_ne!(base, remote_connection_key("source", &changed_password));
    }

    /// Exactly the reported repro: connect, close the sidebar, reopen, connect
    /// again. Every step is driven the way `paint_files_sidebar` drives it,
    /// including the idempotent `TargetChanged` that fires on every frame.
    #[test]
    fn reconnecting_after_hiding_the_panel_reaches_connected_again() {
        let source = unique_source("reconnecting_after_hiding_the_panel_reaches_connected_again");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut state = RemoteFilesState::default();

        let connect_once = |state: &mut RemoteFilesState| {
            let effects = state.transition(RemoteFilesEvent::ConnectRequested);
            let generation = match effects.as_slice() {
                [RemoteFilesEffect::Connect { generation, .. }] => *generation,
                other => panic!("expected a Connect effect, got {:?}", other),
            };
            state.transition(RemoteFilesEvent::Connected {
                generation,
                root: root.clone(),
                listing: listing(&root, &[]),
            });
            generation
        };

        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        connect_once(&mut state);
        assert_eq!(state.phase, RemoteFilesPhase::Connected);

        // Sidebar closed.
        assert_eq!(
            state.transition(RemoteFilesEvent::PanelHidden),
            vec![RemoteFilesEffect::ReleaseLease]
        );
        assert_eq!(state.phase, RemoteFilesPhase::Disconnected);

        // Reopened: the paint path re-announces the same target every frame and
        // must not disturb anything.
        for _ in 0..3 {
            assert!(state
                .transition(RemoteFilesEvent::TargetChanged(Some(target(&source))))
                .is_empty());
        }
        assert_eq!(state.phase, RemoteFilesPhase::Disconnected);

        // Second connect must behave exactly like the first.
        connect_once(&mut state);
        assert_eq!(state.phase, RemoteFilesPhase::Connected);
        assert_eq!(state.root.as_ref(), Some(&root));
    }

    /// A stale reply from the *first* connection must not be able to knock the
    /// second one back into Failed — the generation guard is what prevents the
    /// "reconnect immediately errors" symptom.
    #[test]
    fn a_late_failure_from_the_previous_attempt_is_ignored() {
        let source = unique_source("a_late_failure_from_the_previous_attempt_is_ignored");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));

        let first_generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::PanelHidden);

        let second_generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        assert_ne!(first_generation, second_generation);

        // The abandoned attempt finally fails; it must not touch the live one.
        state.transition(RemoteFilesEvent::ConnectionFailed {
            generation: first_generation,
            message: "stale".to_string(),
        });
        assert_eq!(state.phase, RemoteFilesPhase::Connecting);

        state.transition(RemoteFilesEvent::Connected {
            generation: second_generation,
            root: root.clone(),
            listing: listing(&root, &[]),
        });
        assert_eq!(state.phase, RemoteFilesPhase::Connected);
    }

    /// Connect one source, switch to another, switch back: the first tree must
    /// come straight back from cache instead of the panel dropping to "Not
    /// connected" and asking for a click while the session is still open.
    /// The preview pane widens the sidebar; leaving `selected` set after the
    /// preview closes keeps `right_sidebar_file_preview_active` true, so the
    /// pane's width is never handed back to the terminal.
    #[test]
    fn clearing_the_selection_also_drops_the_preview() {
        let source = unique_source("clear-selection");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let file = root.join_name("a.txt").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(&root, &[("a.txt", RemoteFileKind::File)]),
        });
        state.transition(RemoteFilesEvent::SelectFile(file.clone()));
        assert_eq!(state.selected.as_ref(), Some(&file));
        assert!(!matches!(state.preview, RemotePreviewStatus::None));

        state.clear_selection();
        assert!(state.selected.is_none());
        assert!(matches!(state.preview, RemotePreviewStatus::None));
    }

    #[test]
    fn switching_between_sources_restores_each_tree_from_cache() {
        let first_source = unique_source("switch-a");
        let second_source = unique_source("switch-b");
        let root_a = RemotePath::from_server_absolute("/home/a").unwrap();
        let root_b = RemotePath::from_server_absolute("/home/b").unwrap();
        let mut state = RemoteFilesState::default();

        let connect =
            |state: &mut RemoteFilesState, root: &RemotePath, names: &[(&str, RemoteFileKind)]| {
                let generation = match state
                    .transition(RemoteFilesEvent::ConnectRequested)
                    .as_slice()
                {
                    [RemoteFilesEffect::Connect { generation, .. }] => *generation,
                    other => panic!("expected Connect, got {:?}", other),
                };
                state.transition(RemoteFilesEvent::Connected {
                    generation,
                    root: root.clone(),
                    listing: listing(root, names),
                });
            };

        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&first_source))));
        connect(&mut state, &root_a, &[("only-in-a", RemoteFileKind::File)]);
        assert_eq!(state.rows().len(), 2, "root plus its one entry");

        // Switch away: the second source is unauthorized, so it must ask.
        let effects = state.transition(RemoteFilesEvent::TargetChanged(Some(target(
            &second_source,
        ))));
        assert_eq!(effects, vec![RemoteFilesEffect::ReleaseLease]);
        assert_eq!(state.phase, RemoteFilesPhase::Disconnected);
        connect(&mut state, &root_b, &[]);

        // Switch back: cached, authorized, and already connected — no spinner,
        // no button, and a refresh is kicked off underneath.
        let effects =
            state.transition(RemoteFilesEvent::TargetChanged(Some(target(&first_source))));
        assert_eq!(state.phase, RemoteFilesPhase::Connected);
        assert_eq!(state.root.as_ref(), Some(&root_a));
        assert_eq!(state.rows().len(), 2, "the cached tree is back on screen");
        assert!(
            matches!(
                effects.as_slice(),
                [
                    RemoteFilesEffect::ReleaseLease,
                    RemoteFilesEffect::Connect { .. }
                ]
            ),
            "expected release + background refresh, got {:?}",
            effects
        );
    }

    /// A source whose connection failed must not keep a cached tree: coming
    /// back to it should show the failure, not rows from the dead session.
    #[test]
    fn a_failed_connection_forgets_that_sources_cached_tree() {
        let source = unique_source("failed-forgets");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(&root, &[("f", RemoteFileKind::File)]),
        });

        // Hide (tree goes to cache), come back, then have the refresh fail.
        state.transition(RemoteFilesEvent::PanelHidden);
        state.transition(RemoteFilesEvent::TargetChanged(None));
        let effects = state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let refresh_generation = match effects.as_slice() {
            [_, RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected a background refresh, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::ConnectionFailed {
            generation: refresh_generation,
            message: "connection lost".to_string(),
        });
        assert!(matches!(state.phase, RemoteFilesPhase::Failed(_)));
        assert!(
            state.cached_trees.is_empty(),
            "a dead source must not keep a tree that could be restored later"
        );

        // Switching away and back shows the disconnected state, not old rows.
        state.transition(RemoteFilesEvent::TargetChanged(None));
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        assert_eq!(state.phase, RemoteFilesPhase::Disconnected);
        assert!(state.rows().is_empty());
    }

    #[test]
    fn the_tree_cache_is_bounded_and_evicts_the_oldest_source() {
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut state = RemoteFilesState::default();
        let sources: Vec<String> = (0..REMOTE_TREE_CACHE_CAP + 1)
            .map(|_| unique_source("lru"))
            .collect();

        for source in &sources {
            state.transition(RemoteFilesEvent::TargetChanged(Some(target(source))));
            let generation = match state
                .transition(RemoteFilesEvent::ConnectRequested)
                .as_slice()
            {
                [RemoteFilesEffect::Connect { generation, .. }] => *generation,
                other => panic!("expected Connect, got {:?}", other),
            };
            state.transition(RemoteFilesEvent::Connected {
                generation,
                root: root.clone(),
                listing: listing(&root, &[("f", RemoteFileKind::File)]),
            });
        }
        // The last one is still on screen; the rest are cached up to the cap.
        state.transition(RemoteFilesEvent::TargetChanged(None));
        assert_eq!(state.cached_trees.len(), REMOTE_TREE_CACHE_CAP);
        assert!(
            !state
                .cached_trees
                .contains_key(&(format!("client-domain:{}", sources[0]), "~".to_string())),
            "the oldest source must have been evicted"
        );
    }

    /// Hiding the panel keeps the structure (it is cheap) — the expensive part
    /// is the preview, which the sidebar releases separately. Only the idle
    /// release gives the trees back.
    #[test]
    fn hiding_keeps_the_tree_but_releasing_drops_it() {
        let source = unique_source("hide-keeps");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(&root, &[("f", RemoteFileKind::File)]),
        });

        assert_eq!(
            state.transition(RemoteFilesEvent::PanelHidden),
            vec![RemoteFilesEffect::ReleaseLease]
        );
        assert_eq!(state.cached_trees.len(), 1, "hiding must not drop the tree");

        // Reopening the same target restores it without a reconnect prompt.
        state.transition(RemoteFilesEvent::TargetChanged(None));
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        assert_eq!(state.phase, RemoteFilesPhase::Connected);
        assert_eq!(state.rows().len(), 2);

        // The idle release only ever runs while the panel is hidden, i.e. after
        // `PanelHidden` has already moved the live tree into the cache. Model
        // that order: releasing must not yank a tree the user is looking at.
        state.transition(RemoteFilesEvent::PanelHidden);
        assert_eq!(state.cached_trees.len(), 1);
        state.release_cached_trees();
        assert!(state.cached_trees.is_empty());

        // Reopening now has nothing to restore, so it goes back to asking the
        // connection layer instead of showing stale rows.
        state.transition(RemoteFilesEvent::TargetChanged(None));
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        assert_eq!(state.phase, RemoteFilesPhase::Disconnected);
        assert!(state.rows().is_empty());
        // Still authorized, so the paint path's resume will reconnect without
        // making the user press anything.
        assert!(state.can_resume_current());
        assert!(matches!(
            state
                .transition(RemoteFilesEvent::ResumeRequested)
                .as_slice(),
            [RemoteFilesEffect::Connect {
                allow_connect: true,
                ..
            }]
        ));
    }

    /// Two projects can share one host while browsing different roots; the
    /// cache is keyed by (source, requested root) so project B must never be
    /// shown project A's rows just because the connection is the same.
    #[test]
    fn trees_are_cached_per_root_not_per_source() {
        let source = unique_source("per-root");
        let root_a = RemotePath::from_server_absolute("/srv/a").unwrap();
        let mut state = RemoteFilesState::default();

        state.transition(RemoteFilesEvent::TargetChanged(Some(target_with_root(
            &source, "/srv/a",
        ))));
        let generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root_a.clone(),
            listing: listing(&root_a, &[("only-in-a", RemoteFileKind::File)]),
        });

        // Same source, different root: authorized, but there is nothing cached
        // for this root, so nothing may be restored.
        let effects = state.transition(RemoteFilesEvent::TargetChanged(Some(target_with_root(
            &source, "/srv/b",
        ))));
        assert_eq!(effects, vec![RemoteFilesEffect::ReleaseLease]);
        assert_eq!(state.phase, RemoteFilesPhase::Disconnected);
        assert!(
            state.rows().is_empty(),
            "must not show the other root's rows"
        );

        // Back on the original root the stash is legitimate again.
        state.transition(RemoteFilesEvent::TargetChanged(Some(target_with_root(
            &source, "/srv/a",
        ))));
        assert_eq!(state.phase, RemoteFilesPhase::Connected);
        assert_eq!(state.rows().len(), 2);
    }

    /// While a restored tree waits for its lease, clicks are recorded, not
    /// executed: an effect reaching the handler before the lease exists used
    /// to be reported as a spurious connection failure that wiped the tree.
    #[test]
    fn clicks_on_a_restored_tree_are_deferred_until_the_lease_arrives() {
        let source = unique_source("deferred");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let dir = root.join_name("src").unwrap();
        let file = root.join_name("readme.md").unwrap();
        let mut state = RemoteFilesState::default();

        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(
                &root,
                &[
                    ("src", RemoteFileKind::Directory),
                    ("readme.md", RemoteFileKind::File),
                ],
            ),
        });

        // Stash and restore: the refresh Connect is now in flight.
        state.transition(RemoteFilesEvent::TargetChanged(None));
        let effects = state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let refresh_generation = match effects.as_slice() {
            [_, RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected a background refresh, got {:?}", other),
        };
        assert_eq!(state.phase, RemoteFilesPhase::Connected);

        // Interactions during the round trip must emit nothing...
        assert!(
            state
                .transition(RemoteFilesEvent::ToggleDirectory(dir.clone()))
                .is_empty(),
            "expanding an uncached directory must wait for the lease"
        );
        assert!(
            state
                .transition(RemoteFilesEvent::SelectFile(file.clone()))
                .is_empty(),
            "opening a preview must wait for the lease"
        );
        assert!(
            state.transition(RemoteFilesEvent::Refresh).is_empty(),
            "a refresh is already what the restored tree is waiting on"
        );

        // ...and be flushed, with the current generation, once it lands.
        let effects = state.transition(RemoteFilesEvent::Connected {
            generation: refresh_generation,
            root: root.clone(),
            listing: listing(
                &root,
                &[
                    ("src", RemoteFileKind::Directory),
                    ("readme.md", RemoteFileKind::File),
                ],
            ),
        });
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                RemoteFilesEffect::ListDirectory { path, generation, .. }
                    if *path == dir && *generation == refresh_generation
            )),
            "deferred listing must be flushed, got {:?}",
            effects
        );
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                RemoteFilesEffect::LoadPreview { path, generation, .. }
                    if *path == file && *generation == refresh_generation
            )),
            "deferred preview must be flushed, got {:?}",
            effects
        );
    }

    /// A restored cache can hold up to the whole row budget on its own, and
    /// the refreshed root listing arrives budgeted only against itself — the
    /// union must still respect the global bound.
    #[test]
    fn a_refreshed_root_rebudgets_the_restored_descendants() {
        let source = unique_source("rebudget");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let dir = root.join_name("big").unwrap();
        let mut state = RemoteFilesState::default();

        let wide_listing = |parent: &RemotePath, prefix: &str, count: usize, extra_dir: bool| {
            let mut entries: Vec<RemoteFileEntry> = (0..count)
                .map(|index| {
                    let name = format!("{prefix}-{index}");
                    RemoteFileEntry {
                        path: parent.join_name(&name).unwrap(),
                        name,
                        kind: RemoteFileKind::File,
                        size: None,
                    }
                })
                .collect();
            if extra_dir {
                entries.push(RemoteFileEntry {
                    path: parent.join_name("big").unwrap(),
                    name: "big".to_string(),
                    kind: RemoteFileKind::Directory,
                    size: None,
                });
            }
            RemoteDirectoryListing {
                entries,
                truncated: false,
            }
        };

        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: wide_listing(&root, "root", 999, true),
        });
        state.transition(RemoteFilesEvent::ToggleDirectory(dir.clone()));
        state.transition(RemoteFilesEvent::DirectoryLoaded {
            generation,
            path: dir.clone(),
            listing: wide_listing(&dir, "nested", 1000, false),
        });
        assert_eq!(state.total_entries(), 2000);

        // Stash, restore, and refresh with a root listing that alone fills
        // the budget.
        state.transition(RemoteFilesEvent::TargetChanged(None));
        let effects = state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let refresh_generation = match effects.as_slice() {
            [_, RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected a background refresh, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation: refresh_generation,
            root: root.clone(),
            listing: wide_listing(&root, "fresh", REMOTE_FILE_TREE_ROW_LIMIT, false),
        });
        assert!(
            state.total_entries() <= REMOTE_FILE_TREE_ROW_LIMIT,
            "restored descendants must not defeat the row budget: {} rows",
            state.total_entries()
        );
        assert_eq!(
            state
                .directories
                .get(&root)
                .map(|directory| directory.listing.entries.len()),
            Some(REMOTE_FILE_TREE_ROW_LIMIT),
            "the fresh root listing itself must survive the rebudget"
        );
    }

    #[test]
    fn no_click_never_connects_and_resume_requires_prior_success() {
        let source = unique_source("never-clicked");
        let mut state = RemoteFilesState::default();
        assert_eq!(
            state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source)))),
            vec![RemoteFilesEffect::ReleaseLease]
        );
        assert!(state
            .transition(RemoteFilesEvent::ResumeRequested)
            .is_empty());
        assert_eq!(state.phase, RemoteFilesPhase::Disconnected);
        assert!(matches!(
            state
                .transition(RemoteFilesEvent::ConnectRequested)
                .as_slice(),
            [RemoteFilesEffect::Connect {
                allow_connect: true,
                ..
            }]
        ));
    }

    /// Clicking Connect once authorizes that *source* for the whole process,
    /// not just the window that did it — the SSH session is shared between
    /// windows, so making a second window ask again for a session that is
    /// already open is pure friction.
    #[test]
    fn a_successful_connection_authorizes_the_source_for_every_window() {
        let source = unique_source("shared-auth");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();

        let mut second = RemoteFilesState::default();
        second.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        assert!(
            second
                .transition(RemoteFilesEvent::ResumeRequested)
                .is_empty(),
            "an unauthorized source must never connect on its own"
        );

        let mut first = RemoteFilesState::default();
        first.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match first
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        first.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(&root, &[]),
        });

        // The other window can now resume the same source without a click, and
        // is allowed to redial: once the pool's idle window lapses, silently
        // reconnecting beats demanding a second click for the same host.
        assert!(matches!(
            second
                .transition(RemoteFilesEvent::ResumeRequested)
                .as_slice(),
            [RemoteFilesEffect::Connect {
                allow_connect: true,
                ..
            }]
        ));

        // A different, untouched source is still locked down.
        let other = unique_source("shared-auth-other");
        let mut third = RemoteFilesState::default();
        third.transition(RemoteFilesEvent::TargetChanged(Some(target(&other))));
        assert!(third
            .transition(RemoteFilesEvent::ResumeRequested)
            .is_empty());
    }

    #[test]
    fn collapsing_directory_releases_descendants() {
        let source = unique_source("collapsing_directory_releases_descendants");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let child = root.join_name("src").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state.transition(RemoteFilesEvent::ConnectRequested)[0] {
            RemoteFilesEffect::Connect { generation, .. } => generation,
            _ => unreachable!(),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(&root, &[("src", RemoteFileKind::Directory)]),
        });
        state.transition(RemoteFilesEvent::ToggleDirectory(child.clone()));
        state.transition(RemoteFilesEvent::DirectoryLoaded {
            generation,
            path: child.clone(),
            listing: listing(&child, &[("lib.rs", RemoteFileKind::File)]),
        });
        assert_eq!(state.rows().len(), 3);
        state.transition(RemoteFilesEvent::ToggleDirectory(child));
        assert_eq!(state.rows().len(), 2);
        assert_eq!(state.total_entries(), 1);
    }

    #[test]
    fn stale_directory_result_is_ignored_after_panel_closes() {
        let source = unique_source("stale_directory_result_is_ignored_after_panel_closes");
        let path = RemotePath::from_server_absolute("/home/me/src").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = state.generation;
        state.transition(RemoteFilesEvent::PanelHidden);
        state.transition(RemoteFilesEvent::DirectoryLoaded {
            generation,
            path: path.clone(),
            listing: listing(&path, &[("stale", RemoteFileKind::File)]),
        });
        assert!(state.rows().is_empty());
    }

    #[test]
    fn stale_directory_result_does_not_clear_a_new_loading_request() {
        let source = unique_source("stale_directory_result_does_not_clear_a_new_loading_request");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let child = root.join_name("src").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let first_generation = state.generation;
        state.transition(RemoteFilesEvent::PanelHidden);
        let effect = state.transition(RemoteFilesEvent::ConnectRequested);
        let current_generation = match effect[0] {
            RemoteFilesEffect::Connect { generation, .. } => generation,
            _ => unreachable!(),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation: current_generation,
            root: root.clone(),
            listing: listing(&root, &[("src", RemoteFileKind::Directory)]),
        });
        state.transition(RemoteFilesEvent::ToggleDirectory(child.clone()));
        assert!(state.loading_directories.contains(&child));

        state.transition(RemoteFilesEvent::DirectoryFailed {
            generation: first_generation,
            path: child.clone(),
            message: "stale".to_string(),
        });
        assert!(state.loading_directories.contains(&child));
        assert!(state.error_message.is_none());
    }

    #[test]
    fn resume_does_not_reload_an_already_connected_panel() {
        let source = unique_source("resume_does_not_reload_an_already_connected_panel");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state.transition(RemoteFilesEvent::ConnectRequested)[0] {
            RemoteFilesEffect::Connect { generation, .. } => generation,
            _ => unreachable!(),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(&root, &[]),
        });

        assert!(state
            .transition(RemoteFilesEvent::ResumeRequested)
            .is_empty());
        assert_eq!(state.phase, RemoteFilesPhase::Connected);
        assert_eq!(state.generation, generation);
    }

    #[test]
    fn directory_results_are_capped_to_the_global_row_budget() {
        let source = unique_source("directory_results_are_capped_to_the_global_row_budget");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state.transition(RemoteFilesEvent::ConnectRequested)[0] {
            RemoteFilesEffect::Connect { generation, .. } => generation,
            _ => unreachable!(),
        };
        let entries = (0..REMOTE_FILE_TREE_ROW_LIMIT + 10)
            .map(|index| RemoteFileEntry {
                path: root.join_name(&format!("file-{index}")).unwrap(),
                name: format!("file-{index}"),
                kind: RemoteFileKind::File,
                size: None,
            })
            .collect();
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root,
            listing: RemoteDirectoryListing {
                entries,
                truncated: true,
            },
        });
        // One synthetic root row plus at most the configured remote entries.
        assert_eq!(state.rows().len(), REMOTE_FILE_TREE_ROW_LIMIT + 1);
        assert!(state.has_truncated_directory());
    }
}
