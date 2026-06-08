use crate::session::{DeadSession, SessionRequest, SessionSender, SignalChannel};
use crate::sessioninner::{ChannelId, ChannelInfo, DescriptorState};
use crate::sessionwrap::SessionWrap;
use filedescriptor::{socketpair, FileDescriptor};
use portable_pty::{ExitStatus, PtySize};
use smol::channel::{bounded, Receiver, TryRecvError, TrySendError};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RESIZE_RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Debug)]
pub(crate) struct NewPty {
    pub term: String,
    pub size: PtySize,
    pub command_line: Option<String>,
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug)]
pub(crate) struct ResizePty {
    pub channel: ChannelId,
    pub size: PtySize,
}

#[derive(Debug)]
pub struct SshPty {
    pub(crate) channel: ChannelId,
    pub(crate) tx: Option<SessionSender>,
    pub(crate) reader: FileDescriptor,
    pub(crate) writer: FileDescriptor,
    pub(crate) size: Mutex<PtySize>,
    pending_resize: Arc<Mutex<Option<PtySize>>>,
    resize_retry_scheduled: Arc<AtomicBool>,
}

impl std::io::Write for SshPty {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

impl SshPty {
    fn schedule_resize_retry(&self, tx: SessionSender) {
        if self.resize_retry_scheduled.swap(true, Ordering::AcqRel) {
            return;
        }

        let pending_resize = Arc::clone(&self.pending_resize);
        let retry_scheduled = Arc::clone(&self.resize_retry_scheduled);
        let channel = self.channel;
        std::thread::spawn(move || loop {
            std::thread::sleep(RESIZE_RETRY_DELAY);
            let Some(size) = pending_resize.lock().unwrap().take() else {
                retry_scheduled.store(false, Ordering::Release);
                if pending_resize.lock().unwrap().is_none() {
                    return;
                }
                if retry_scheduled.swap(true, Ordering::AcqRel) {
                    return;
                }
                continue;
            };

            match try_send_resize(&tx, channel, size) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    restore_pending_resize_if_missing(&pending_resize, size);
                }
                Err(TrySendError::Closed(_)) => {
                    retry_scheduled.store(false, Ordering::Release);
                    return;
                }
            }
        });
    }
}

impl portable_pty::MasterPty for SshPty {
    fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        *self.size.lock().unwrap() = size;
        let Some(tx) = self.tx.as_ref() else {
            return Err(DeadSession.into());
        };
        match try_send_resize(tx, self.channel, size) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // Resize events are coalescable and can arrive in bursts during
                // live resizing or UI view transitions. Keep the latest size
                // and retry it so the remote PTY doesn't get stuck at stale
                // rows/cols when the burst stops.
                *self.pending_resize.lock().unwrap() = Some(size);
                self.schedule_resize_retry(tx.clone());
                log::debug!(
                    "coalescing SSH resize for channel {} because the request queue is full",
                    self.channel
                );
            }
            Err(TrySendError::Closed(_)) => return Err(DeadSession.into()),
        }
        Ok(())
    }

    fn get_size(&self) -> anyhow::Result<PtySize> {
        Ok(*self.size.lock().unwrap())
    }

    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send + 'static>> {
        let reader = self.reader.try_clone()?;
        Ok(Box::new(reader))
    }

    fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send + 'static>> {
        let writer = self.writer.try_clone()?;
        Ok(Box::new(writer))
    }

    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<i32> {
        // It's not local, so there's no meaningful leader
        None
    }

    #[cfg(unix)]
    fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    #[cfg(unix)]
    fn tty_name(&self) -> Option<std::path::PathBuf> {
        None
    }
}

fn try_send_resize(
    tx: &SessionSender,
    channel: ChannelId,
    size: PtySize,
) -> Result<(), TrySendError<SessionRequest>> {
    tx.try_send_request(SessionRequest::ResizePty(ResizePty { channel, size }, None))
}

fn restore_pending_resize_if_missing(pending_resize: &Mutex<Option<PtySize>>, size: PtySize) {
    let mut pending = pending_resize.lock().unwrap();
    if pending.is_none() {
        *pending = Some(size);
    }
}

#[derive(Debug)]
pub struct SshChildProcess {
    pub(crate) channel: ChannelId,
    pub(crate) tx: Option<SessionSender>,
    pub(crate) exit: Receiver<ExitStatus>,
    pub(crate) exited: Option<ExitStatus>,
}

impl SshChildProcess {
    pub async fn async_wait(&mut self) -> std::io::Result<ExitStatus> {
        if let Some(status) = self.exited.as_ref() {
            return Ok(status.clone());
        }
        match self.exit.recv().await {
            Ok(status) => {
                self.exited.replace(status.clone());
                Ok(status)
            }
            Err(_) => {
                let status = ExitStatus::with_exit_code(1);
                self.exited.replace(status.clone());
                Ok(status)
            }
        }
    }
}

impl portable_pty::Child for SshChildProcess {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        if let Some(status) = self.exited.as_ref() {
            return Ok(Some(status.clone()));
        }
        match self.exit.try_recv() {
            Ok(status) => {
                self.exited.replace(status.clone());
                Ok(Some(status))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Closed) => {
                let status = ExitStatus::with_exit_code(1);
                self.exited.replace(status.clone());
                Ok(Some(status))
            }
        }
    }

    fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
        if let Some(status) = self.exited.as_ref() {
            return Ok(status.clone());
        }
        match smol::block_on(self.exit.recv()) {
            Ok(status) => {
                self.exited.replace(status.clone());
                Ok(status)
            }
            Err(_) => {
                let status = ExitStatus::with_exit_code(1);
                self.exited.replace(status.clone());
                Ok(status)
            }
        }
    }

    fn process_id(&self) -> Option<u32> {
        None
    }

    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        None
    }
}

impl portable_pty::ChildKiller for SshChildProcess {
    fn kill(&mut self) -> std::io::Result<()> {
        if let Some(tx) = self.tx.as_ref() {
            tx.try_send(SessionRequest::SignalChannel(SignalChannel {
                channel: self.channel,
                signame: "HUP",
            }))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        }
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        Box::new(SshChildKiller {
            tx: self.tx.clone(),
            channel: self.channel,
        })
    }
}

#[derive(Debug, Clone)]
struct SshChildKiller {
    pub(crate) tx: Option<SessionSender>,
    pub(crate) channel: ChannelId,
}

impl portable_pty::ChildKiller for SshChildKiller {
    fn kill(&mut self) -> std::io::Result<()> {
        if let Some(tx) = self.tx.as_ref() {
            tx.try_send(SessionRequest::SignalChannel(SignalChannel {
                channel: self.channel,
                signame: "HUP",
            }))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        }
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        Box::new(SshChildKiller {
            tx: self.tx.clone(),
            channel: self.channel,
        })
    }
}

impl crate::sessioninner::SessionInner {
    pub fn new_pty(
        &mut self,
        sess: &mut SessionWrap,
        newpty: NewPty,
    ) -> anyhow::Result<(SshPty, SshChildProcess)> {
        sess.set_blocking(true);

        let mut channel = sess.open_session()?;

        if let Some("yes") = self.config.get("forwardagent").map(|s| s.as_str()) {
            if self.identity_agent().is_some() {
                if let Err(err) = channel.request_auth_agent_forwarding() {
                    log::error!("Failed to request agent forwarding: {:#}", err);
                }
            }
        }

        channel.request_pty(&newpty)?;

        if let Some(env) = &newpty.env {
            for (key, val) in env {
                if let Err(err) = channel.request_env(key, val) {
                    // Depending on the server configuration, a given
                    // setenv request may not succeed, but that doesn't
                    // prevent the connection from being set up.
                    if !self.shown_accept_env_error {
                        log::warn!(
                            "ssh: setenv {}={} failed: {}. \
                            Check the AcceptEnv setting on the ssh server side. \
                            Additional errors with setting env vars in this \
                            session will be logged at debug log level.",
                            key,
                            val,
                            err
                        );
                        self.shown_accept_env_error = true;
                    } else {
                        log::debug!(
                            "ssh: setenv {}={} failed: {}. \
                             Check the AcceptEnv setting on the ssh server side.",
                            key,
                            val,
                            err
                        );
                    }
                }
            }
        }

        if let Some(cmd) = &newpty.command_line {
            channel.request_exec(cmd)?;
        } else {
            channel.request_shell()?;
        }

        let channel_id = self.next_channel_id;
        self.next_channel_id += 1;

        let (write_to_stdin, mut read_from_stdin) = socketpair()?;
        let (mut write_to_stdout, read_from_stdout) = socketpair()?;
        let write_to_stderr = write_to_stdout.try_clone()?;

        read_from_stdin.set_non_blocking(true)?;
        write_to_stdout.set_non_blocking(true)?;

        let ssh_pty = SshPty {
            channel: channel_id,
            tx: None,
            reader: read_from_stdout,
            writer: write_to_stdin,
            size: Mutex::new(newpty.size),
            pending_resize: Arc::new(Mutex::new(None)),
            resize_retry_scheduled: Arc::new(AtomicBool::new(false)),
        };

        let (exit_tx, exit_rx) = bounded(1);

        let child = SshChildProcess {
            channel: channel_id,
            tx: None,
            exit: exit_rx,
            exited: None,
        };

        let info = ChannelInfo {
            channel_id,
            channel,
            exit: Some(exit_tx),
            exited: false,
            descriptors: [
                DescriptorState {
                    fd: Some(read_from_stdin),
                    buf: VecDeque::with_capacity(8192),
                },
                DescriptorState {
                    fd: Some(write_to_stdout),
                    buf: VecDeque::with_capacity(8192),
                },
                DescriptorState {
                    fd: Some(write_to_stderr),
                    buf: VecDeque::with_capacity(8192),
                },
            ],
        };

        self.channels.insert(channel_id, info);

        Ok((ssh_pty, child))
    }

    pub fn resize_pty(&mut self, resize: ResizePty) -> anyhow::Result<()> {
        let info = self
            .channels
            .get_mut(&resize.channel)
            .ok_or_else(|| anyhow::anyhow!("invalid channel id {}", resize.channel))?;
        info.channel.resize_pty(&resize)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionRequest, SessionSender};
    use portable_pty::MasterPty;
    use smol::channel::bounded;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn resize_retries_latest_request_when_session_queue_is_full() {
        let (tx, rx) = bounded(1);
        tx.try_send(SessionRequest::SessionDropped).unwrap();
        let (pipe_write, _pipe_read) = socketpair().unwrap();
        let (writer, reader) = socketpair().unwrap();
        let pty = SshPty {
            channel: 7,
            tx: Some(SessionSender {
                tx,
                pipe: Arc::new(Mutex::new(pipe_write)),
            }),
            reader,
            writer,
            size: Mutex::new(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 480,
            }),
            pending_resize: Arc::new(Mutex::new(None)),
            resize_retry_scheduled: Arc::new(AtomicBool::new(false)),
        };

        let size = PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 960,
            pixel_height: 800,
        };

        pty.resize(size).unwrap();
        assert_eq!(*pty.size.lock().unwrap(), size);

        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionRequest::SessionDropped
        ));
        let mut retried = None;
        for _ in 0..40 {
            if let Ok(request) = rx.try_recv() {
                retried = Some(request);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let Some(SessionRequest::ResizePty(resize, None)) = retried else {
            panic!("expected retried resize request");
        };
        assert_eq!(resize.channel, 7);
        assert_eq!(resize.size, size);
    }

    #[test]
    fn resize_retry_restore_preserves_newer_pending_request() {
        let old_size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 640,
            pixel_height: 480,
        };
        let newer_size = PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 960,
            pixel_height: 800,
        };

        let pending = Mutex::new(Some(newer_size));
        restore_pending_resize_if_missing(&pending, old_size);
        assert_eq!(*pending.lock().unwrap(), Some(newer_size));

        let pending = Mutex::new(None);
        restore_pending_resize_if_missing(&pending, old_size);
        assert_eq!(*pending.lock().unwrap(), Some(old_size));
    }
}
