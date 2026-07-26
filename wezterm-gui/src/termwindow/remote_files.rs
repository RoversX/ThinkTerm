use crate::workspace_threads::{RemoteFilesSource, RemoteFilesTarget};
use config::SshDomain;
use smol::channel::Sender;
use smol::io::AsyncReadExt;
use std::collections::{HashMap, HashSet};
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
            let home = sftp
                .canonicalize(".")
                .await
                .map_err(|err| format!("Unable to resolve remote home: {err}"))?;
            let home = RemotePath::from_server_absolute(home.as_str())?;
            resolve_requested_root(home, &requested)
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
        loop {
            let waiter = {
                let mut inner = self.inner.lock().unwrap();
                match inner.entries.get_mut(&key) {
                    Some(ManagedConnection::Ready {
                        connection_id,
                        backend,
                        leases,
                        idle_generation,
                        idle_since,
                    }) => {
                        *leases = leases.saturating_add(1);
                        *idle_generation = idle_generation.wrapping_add(1);
                        *idle_since = None;
                        return Ok(RemoteConnectionLease {
                            key,
                            backend: Arc::clone(backend),
                            manager: Arc::downgrade(self),
                            connection_id: *connection_id,
                        });
                    }
                    Some(ManagedConnection::Connecting { waiters }) => {
                        let (tx, rx) = smol::channel::bounded(1);
                        waiters.push(tx);
                        Some(rx)
                    }
                    None if !allow_connect => return Err(RemoteAcquireError::NotConnected),
                    None => {
                        inner.entries.insert(
                            key.clone(),
                            ManagedConnection::Connecting { waiters: vec![] },
                        );
                        None
                    }
                }
            };

            if let Some(waiter) = waiter {
                match waiter.recv().await {
                    Ok(Ok(())) => continue,
                    Ok(Err(err)) => return Err(RemoteAcquireError::Failed(err)),
                    Err(_) => {
                        return Err(RemoteAcquireError::Failed(
                            "Remote Files connection was canceled".to_string(),
                        ))
                    }
                }
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

    pub(crate) fn invalidate(&self, key: &str) {
        self.inner.lock().unwrap().entries.remove(key);
    }
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

pub(crate) fn invalidate_remote_connection_if_dead(key: &str, message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    let dead = lower.contains("session is dead")
        || lower.contains("channel is closed")
        || lower.contains("channel closed")
        || lower.contains("sending on a closed")
        || lower.contains("no connection has been set up")
        || lower.contains("connection, but we lost it")
        || lower.contains("failed to send request")
        || lower.contains("failed to receive response")
        || lower.contains("broken pipe");
    if dead {
        remote_connection_manager().invalidate(key);
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
    authorized_sources: HashSet<String>,
}

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
            authorized_sources: HashSet::new(),
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

    pub(crate) fn can_resume_current(&self) -> bool {
        self.current_source_key()
            .is_some_and(|key| self.authorized_sources.contains(&key))
    }

    pub(crate) fn transition(&mut self, event: RemoteFilesEvent) -> Vec<RemoteFilesEffect> {
        match event {
            RemoteFilesEvent::TargetChanged(target) => {
                if self.target == target {
                    return Vec::new();
                }
                self.clear_loaded_data();
                self.target = target;
                vec![RemoteFilesEffect::ReleaseLease]
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
                    self.authorized_sources.insert(key);
                }
                self.phase = RemoteFilesPhase::Connected;
                self.error_message = None;
                self.root = Some(root.clone());
                self.expanded.insert(root.clone());
                if listing.entries.len() > REMOTE_FILE_TREE_ROW_LIMIT {
                    listing.entries.truncate(REMOTE_FILE_TREE_ROW_LIMIT);
                    listing.truncated = true;
                }
                self.directories.insert(root, RemoteDirectory { listing });
                Vec::new()
            }
            RemoteFilesEvent::ConnectionFailed {
                generation,
                message,
            } => {
                if generation == self.generation {
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
                self.clear_loaded_data();
                vec![RemoteFilesEffect::ReleaseLease]
            }
        }
    }

    fn begin_connect(&mut self, allow_connect: bool) -> Vec<RemoteFilesEffect> {
        let Some(target) = self.target.clone() else {
            return Vec::new();
        };
        let source_key = Self::source_key(&target.source);
        if !allow_connect && !self.authorized_sources.contains(&source_key) {
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
            allow_connect,
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

    fn clear_loaded_data(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.phase = RemoteFilesPhase::Disconnected;
        self.root = None;
        self.directories.clear();
        self.expanded.clear();
        self.loading_directories.clear();
        self.selected = None;
        self.preview = RemotePreviewStatus::None;
        self.error_message = None;
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn target(source: &str) -> RemoteFilesTarget {
        RemoteFilesTarget {
            project_id: "project".to_string(),
            project_name: "Project".to_string(),
            source: RemoteFilesSource::ClientDomain(source.to_string()),
            requested_root: "~".to_string(),
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

    #[test]
    fn no_click_never_connects_and_resume_requires_prior_success() {
        let mut state = RemoteFilesState::default();
        assert_eq!(
            state.transition(RemoteFilesEvent::TargetChanged(Some(target("mux")))),
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

    #[test]
    fn successful_connection_authorizes_only_that_window_state() {
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut first = RemoteFilesState::default();
        first.transition(RemoteFilesEvent::TargetChanged(Some(target("mux"))));
        let effect = first.transition(RemoteFilesEvent::ConnectRequested);
        let generation = match effect[0] {
            RemoteFilesEffect::Connect { generation, .. } => generation,
            _ => unreachable!(),
        };
        first.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(&root, &[]),
        });
        first.transition(RemoteFilesEvent::PanelHidden);
        assert!(matches!(
            first
                .transition(RemoteFilesEvent::ResumeRequested)
                .as_slice(),
            [RemoteFilesEffect::Connect {
                allow_connect: false,
                ..
            }]
        ));

        let mut second = RemoteFilesState::default();
        second.transition(RemoteFilesEvent::TargetChanged(Some(target("mux"))));
        assert!(second
            .transition(RemoteFilesEvent::ResumeRequested)
            .is_empty());
    }

    #[test]
    fn collapsing_directory_releases_descendants() {
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let child = root.join_name("src").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target("mux"))));
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
        let path = RemotePath::from_server_absolute("/home/me/src").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target("mux"))));
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
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let child = root.join_name("src").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target("mux"))));
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
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target("mux"))));
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
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target("mux"))));
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
