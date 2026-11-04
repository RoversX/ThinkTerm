//! The only production caller of `Approval::authenticated`.
//!
//! Helpers are packaged next to the executable, started by the authority,
//! and answer through an inherited anonymous pipe. Neither a mux PDU nor a
//! user-supplied executable path can complete an authentication transaction.

use crate::authorization::{ApprovalCancellation, Monitor};
use crate::text::Text;
use crate::{Approval, Authority, Error, GrantInfo, GrantScope, Permission, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    kind: String,
    id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Intent {
    id: String,
    source: String,
    targets: String,
    input: bool,
    seconds: u64,
    text: Text,
}

fn id_text(id: crate::Id) -> String {
    id.as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn read_event(reader: &mut impl BufRead) -> Result<Event> {
    let mut line = Vec::new();
    reader
        .take(1025)
        .read_until(b'\n', &mut line)
        .map_err(|_| Error::Unavailable)?;
    if line.len() > 1024 || line.last() != Some(&b'\n') {
        return Err(Error::Protocol);
    }
    serde_json::from_slice(&line).map_err(|_| Error::Protocol)
}

fn helper_command(mode: &str) -> Result<Command> {
    let executable = std::env::current_exe().map_err(|_| Error::Unavailable)?;
    let directory = executable.parent().ok_or(Error::Unavailable)?;
    let helper: PathBuf = directory.join(if cfg!(windows) {
        "thinkterm-connect-auth.exe"
    } else {
        "thinkterm-connect-auth"
    });
    if !helper.is_file() {
        return Err(Error::Unavailable);
    }
    let mut command = Command::new(helper);
    command
        .arg(mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Do not inherit loader hooks, language startup files, or custom toolkit
    // modules from the configuration environment.
    command.env_clear();
    for key in [
        "SystemRoot",
        "WINDIR",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_RUNTIME_DIR",
        "XDG_SESSION_ID",
        "HOME",
        "TMPDIR",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "en_US.UTF-8");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW; the helper creates its own dialog.
    }
    Ok(command)
}

fn kill(child: &Mutex<Child>) {
    if let Ok(mut child) = child.lock() {
        #[cfg(unix)]
        if matches!(child.try_wait(), Ok(None)) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = child.kill();
    }
}

fn reap(child: &Mutex<Child>) {
    kill(child);
    if let Ok(mut child) = child.lock() {
        let _ = child.wait();
    }
}

pub struct NativeSession {
    authority: Arc<Authority>,
    monitor: Arc<Monitor>,
    child: Arc<Mutex<Child>>,
}

impl NativeSession {
    pub fn start(authority: Arc<Authority>) -> Result<Self> {
        let mut child = helper_command("--monitor")?
            .spawn()
            .map_err(|_| Error::Unavailable)?;
        let stdout = child.stdout.take().ok_or(Error::Unavailable)?;
        let child = Arc::new(Mutex::new(child));
        let (sender, receiver) = mpsc::sync_channel(1);
        let (monitor_sender, monitor_receiver) = mpsc::sync_channel::<Arc<Monitor>>(1);
        let child_worker = child.clone();
        std::thread::Builder::new()
            .name("connect-session-monitor".into())
            .spawn(move || {
                let mut reader = BufReader::with_capacity(1024, stdout);
                let result: Result<()> = (|| {
                    let event = read_event(&mut reader)?;
                    if event.kind != "ready" || event.id.is_some() {
                        return Err(Error::Protocol);
                    }
                    sender.send(Ok(())).map_err(|_| Error::Unavailable)?;
                    let monitor = monitor_receiver
                        .recv_timeout(Duration::from_secs(5))
                        .map_err(|_| Error::Unavailable)?;
                    let result = (|| loop {
                        let event = read_event(&mut reader)?;
                        if event.kind != "alive" || event.id.is_some() {
                            return Err(Error::Unavailable);
                        }
                        monitor.heartbeat()?;
                    })();
                    monitor.close();
                    result
                })();
                if let Err(error) = result {
                    let _ = sender.try_send(Err(error));
                }
                reap(&child_worker);
            })
            .map_err(|_| {
                reap(&child);
                Error::Unavailable
            })?;
        if !matches!(receiver.recv_timeout(Duration::from_secs(5)), Ok(Ok(()))) {
            kill(&child);
            return Err(Error::Unavailable);
        }
        let monitor = Arc::new(authority.start_monitor().map_err(|error| {
            kill(&child);
            error
        })?);
        monitor_sender.send(monitor.clone()).map_err(|_| {
            kill(&child);
            Error::Unavailable
        })?;
        Ok(Self {
            authority,
            monitor,
            child,
        })
    }

    /// Source is display text from the verified SSH session. Scope, duration,
    /// and transaction identity are read from the frozen internal approval.
    /// The language picks a translation of what the window says about them.
    pub fn authenticate(
        &self,
        approval: Approval,
        source: &str,
        language: &str,
    ) -> Result<NativeAttempt> {
        self.monitor.check()?;
        if source.is_empty() || source.len() > 256 || source.chars().any(char::is_control) {
            return Err(Error::Protocol);
        }
        let details = approval.details()?;
        let cancellation = approval.cancellation();
        let input = details.permission == Permission::ReadAndInput;
        let (text, all_terminals) = Text::new(
            language,
            source,
            details.scope == GrantScope::AllLocalTerminals,
            input,
            details.seconds,
        );
        let intent = Intent {
            id: id_text(details.id),
            source: source.to_string(),
            targets: match details.scope {
                GrantScope::AllLocalTerminals => all_terminals.into(),
                GrantScope::SelectedTerminals => details
                    .targets
                    .iter()
                    .map(|target| format!("{} [{}]", target.pane, id_text(target.incarnation)))
                    .collect::<Vec<_>>()
                    .join("\n"),
            },
            input,
            seconds: details.seconds,
            text,
        };
        let mut encoded = serde_json::to_vec(&intent).map_err(|_| Error::Protocol)?;
        if encoded.len() >= 8192 {
            return Err(Error::TooLarge);
        }
        encoded.push(b'\n');
        let mut child = helper_command("--authenticate")?
            .spawn()
            .map_err(|_| Error::Unavailable)?;
        let mut stdin = child.stdin.take().ok_or(Error::Unavailable)?;
        let stdout = child.stdout.take().ok_or(Error::Unavailable)?;
        let child = Arc::new(Mutex::new(child));
        let cancelled = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel(1);
        let (finished_sender, finished_receiver) = mpsc::sync_channel::<()>(1);
        let deadline_child = child.clone();
        let deadline_cancelled = cancelled.clone();
        let deadline_monitor = self.monitor.clone();
        std::thread::Builder::new()
            .name("connect-auth-deadline".into())
            .spawn(move || {
                let started = Instant::now();
                loop {
                    match finished_receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(()) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            deadline_cancelled.store(true, Ordering::SeqCst);
                            kill(&deadline_child);
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if started.elapsed() < Duration::from_secs(crate::AUTH_TIMEOUT_SECS)
                        && deadline_monitor.check().is_ok()
                        && !deadline_cancelled.load(Ordering::SeqCst)
                    {
                        continue;
                    }
                    deadline_cancelled.store(true, Ordering::SeqCst);
                    kill(&deadline_child);
                    break;
                }
            })
            .map_err(|_| {
                reap(&child);
                Error::Unavailable
            })?;
        let child_worker = child.clone();
        let cancelled_worker = cancelled.clone();
        let authority = self.authority.clone();
        std::thread::Builder::new()
            .name("connect-native-auth".into())
            .spawn(move || {
                let result = (|| {
                    stdin.write_all(&encoded).map_err(|_| Error::Unavailable)?;
                    let mut reader = BufReader::with_capacity(1024, stdout);
                    let event = read_event(&mut reader)?;
                    if event.kind == "rejected" && event.id.as_deref() == Some(&intent.id) {
                        return Err(Error::Cancelled);
                    }
                    if event.kind != "approved"
                        || event.id.as_deref() != Some(&intent.id)
                        || cancelled_worker.load(Ordering::SeqCst)
                    {
                        return Err(Error::Unavailable);
                    }
                    let started = Instant::now();
                    loop {
                        let status = child_worker
                            .lock()
                            .map_err(|_| Error::Unavailable)?
                            .try_wait()
                            .map_err(|_| Error::Unavailable)?;
                        if let Some(status) = status {
                            if !status.success() {
                                return Err(Error::Unavailable);
                            }
                            break;
                        }
                        if started.elapsed() >= Duration::from_secs(2)
                            || cancelled_worker.load(Ordering::SeqCst)
                        {
                            return Err(Error::Timeout);
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let info = approval.authenticated()?;
                    Ok(AuthorizedGrant { info, authority })
                })();
                reap(&child_worker);
                let _ = finished_sender.send(());
                let _ = sender.send(result);
                // Keep the helper's control input open until it has completed.
                drop(stdin);
            })
            .map_err(|_| {
                reap(&child);
                Error::Unavailable
            })?;
        Ok(NativeAttempt {
            child,
            cancelled,
            cancellation,
            receiver,
            adopted: AtomicBool::new(false),
        })
    }
}

impl Drop for NativeSession {
    fn drop(&mut self) {
        self.monitor.close();
        kill(&self.child);
    }
}

pub struct NativeAttempt {
    child: Arc<Mutex<Child>>,
    cancelled: Arc<AtomicBool>,
    cancellation: ApprovalCancellation,
    receiver: mpsc::Receiver<Result<AuthorizedGrant>>,
    adopted: AtomicBool,
}

impl NativeAttempt {
    pub fn try_result(&self) -> Result<Option<AuthorizedGrant>> {
        match self.receiver.try_recv() {
            Ok(result) => result.map(|grant| {
                self.adopted.store(true, Ordering::SeqCst);
                Some(grant)
            }),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(Error::Unavailable),
        }
    }
}

impl Drop for NativeAttempt {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if !self.adopted.load(Ordering::SeqCst) {
            self.cancellation.cancel();
        }
        kill(&self.child);
    }
}

/// Dropping an undelivered result or an abandoned UI grant revokes it.
pub struct AuthorizedGrant {
    info: GrantInfo,
    authority: Arc<Authority>,
}

impl AuthorizedGrant {
    pub fn info(&self) -> &GrantInfo {
        &self.info
    }
}

impl Drop for AuthorizedGrant {
    fn drop(&mut self) {
        let _ = self.authority.revoke(self.info.grant);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_output_is_bounded_and_strict() {
        assert_eq!(
            read_event(&mut &b"{\"kind\":\"ready\",\"id\":null}\n"[..])
                .unwrap()
                .kind,
            "ready"
        );
        assert!(read_event(&mut &b"{\"kind\":\"approved\",\"success\":true}\n"[..]).is_err());
        assert!(read_event(&mut &b"{\"kind\":\"ready\"}"[..]).is_err());
        assert!(read_event(&mut vec![b'x'; 1025].as_slice()).is_err());
    }
}
