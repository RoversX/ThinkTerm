use super::{model::Manifest, source};
use anyhow::{ensure, Context, Result};
use rustix::net::{recvmsg, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{IoSliceMut, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

pub struct Transfer {
    pub manifest: Manifest,
    pub fds: Vec<OwnedFd>,
    stream: UnixStream,
    _relay: tempfile::TempDir,
    response: Receiver<Result<serde_json::Value>>,
    committed: bool,
}

#[derive(Deserialize, Serialize)]
struct Relay {
    nonce: String,
    socket: String,
    token: String,
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Runs before configuration or GUI initialization in both owning binaries.
pub fn maybe_run_relay() -> Option<Result<()>> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).and_then(|s| s.to_str()) != Some("--herdr-import-relay") {
        return None;
    }
    Some((|| {
        ensure!(args.len() == 6, "Invalid Herdr import relay arguments");
        let mut stream = UnixStream::connect(&args[2])?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let relay = Relay {
            nonce: args[3].to_str().context("Invalid nonce")?.into(),
            socket: args[4].to_str().context("Invalid handoff path")?.into(),
            token: args[5]
                .to_str()
                .context("Invalid handoff authorization")?
                .into(),
        };
        serde_json::to_writer(&mut stream, &relay)?;
        stream.write_all(b"\n")?;
        // Keep the socket connected until the owner has read it. On macOS
        // setting SO_RCVTIMEO on an already-disconnected accepted socket
        // can fail even though its receive buffer still holds the message.
        let mut ack = [0];
        stream.read_exact(&mut ack)?;
        ensure!(ack == [b'K'], "Herdr relay was not accepted");
        Ok(())
    })())
}

pub fn receive_fds(stream: &UnixStream, expected: usize) -> Result<Vec<OwnedFd>> {
    let mut fds = Vec::with_capacity(expected);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = RecvFlags::CMSG_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flags = RecvFlags::empty();
    while fds.len() < expected {
        let mut storage = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(64))];
        let mut ancillary = RecvAncillaryBuffer::new(&mut storage);
        let mut data = [0u8];
        let received = recvmsg(
            stream,
            &mut [IoSliceMut::new(&mut data)],
            &mut ancillary,
            flags,
        )?;
        let before = fds.len();
        for message in ancillary.drain() {
            if let RecvAncillaryMessage::ScmRights(batch) = message {
                for fd in batch {
                    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
                    fds.push(fd);
                }
            }
        }
        ensure!(
            !received.flags.contains(ReturnFlags::CTRUNC),
            "Truncated Herdr descriptor batch"
        );
        ensure!(
            received.bytes == 1 && data[0] == b'F' && fds.len() > before && fds.len() <= expected,
            "Invalid Herdr descriptor batch"
        );
    }
    for fd in &fds {
        ensure!(
            rustix::termios::isatty(fd),
            "Herdr supplied a non-terminal descriptor"
        );
    }
    Ok(fds)
}

pub fn receive(dir: &Path, exe: &Path) -> Result<Transfer> {
    let relay_dir = tempfile::Builder::new()
        .prefix("tt-hi-")
        .tempdir_in("/tmp")?;
    fs::set_permissions(relay_dir.path(), fs::Permissions::from_mode(0o700))?;
    let rendezvous = relay_dir.path().join("relay.sock");
    let listener = UnixListener::bind(&rendezvous)?;
    listener.set_nonblocking(true)?;
    let nonce = uuid::Uuid::new_v4().to_string();
    let script = relay_dir.path().join("receive");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nexec {} --herdr-import-relay {} {} \"$3\" \"$4\"\n",
            quote(exe.to_str().context("Executable path is not UTF-8")?),
            quote(rendezvous.to_str().unwrap()),
            quote(&nonce)
        ),
    )?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
    let (sender, response) = mpsc::sync_channel(1);
    let source_dir = dir.to_owned();
    std::thread::Builder::new()
        .name("herdr-handoff-request".into())
        .spawn(move || {
            let result = source::request(
                &source_dir,
                "server.live_handoff",
                serde_json::json!({"import_exe": script}),
                Duration::from_secs(120),
            );
            let _ = sender.send(result);
        })?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut relay_stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if let Ok(result) = response.try_recv() {
                    result?;
                    anyhow::bail!("Herdr ended the handoff without a receiver");
                }
                ensure!(
                    Instant::now() < deadline,
                    "Herdr did not start its handoff receiver"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(err.into()),
        }
    };
    relay_stream
        .set_nonblocking(false)
        .context("Set Herdr relay blocking mode")?;
    relay_stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .context("Set Herdr relay timeout")?;
    let relay: Relay = serde_json::from_slice(&source::read_line(&mut relay_stream, 16384)?)?;
    ensure!(relay.nonce == nonce, "Herdr relay authorization failed");
    relay_stream.write_all(b"K")?;
    let socket = Path::new(&relay.socket);
    ensure!(
        socket
            .parent()
            .context("Invalid Herdr handoff path")?
            .canonicalize()?
            == dir.canonicalize()?
            && socket
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with("herdr-handoff-") && s.ends_with(".sock")),
        "Unexpected Herdr handoff socket"
    );
    ensure!(
        !relay.token.contains(['\n', '\r']) && relay.token.len() <= 256,
        "Invalid Herdr handoff authorization"
    );
    let mut stream = UnixStream::connect(socket).context("Connect to Herdr handoff socket")?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    writeln!(stream, "{}", relay.token).context("Authorize Herdr handoff")?;
    let manifest: Manifest =
        serde_json::from_slice(&source::read_line(&mut stream, source::MAX_BYTES)?)?;
    manifest.validate()?;
    stream.write_all(b"validated\n")?;
    let fds =
        receive_fds(&stream, manifest.panes.len()).context("Receive Herdr terminal descriptors")?;
    Ok(Transfer {
        manifest,
        fds,
        stream,
        _relay: relay_dir,
        response,
        committed: false,
    })
}

impl Transfer {
    pub fn commit(&mut self) -> Result<()> {
        if !self.committed {
            commit_stream(&mut self.stream)?;
            self.committed = true;
        }
        Ok(())
    }

    pub fn finish(self) {
        // The request thread has a finite socket timeout and does not own PTYs.
        if let Ok(Err(err)) = self.response.try_recv() {
            log::warn!("Herdr handoff response after commit: {err:#}");
        }
    }
}

fn commit_stream(stream: &mut UnixStream) -> Result<()> {
    stream.write_all(b"restored\nready\n")?;
    ensure!(
        source::read_line(stream, 128)? == b"committed",
        "Herdr did not commit the transfer"
    );
    // Herdr releases its runtimes after sending committed. Losing the final
    // receipt must not discard the only remaining copies of the PTYs.
    let _ = stream.write_all(b"owned\n");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_transfer_survives_lost_final_receipt() {
        let (mut receiver, mut source) = UnixStream::pair().unwrap();
        let peer = std::thread::spawn(move || {
            assert_eq!(source::read_line(&mut source, 128).unwrap(), b"restored");
            assert_eq!(source::read_line(&mut source, 128).unwrap(), b"ready");
            source.shutdown(std::net::Shutdown::Read).unwrap();
            source.write_all(b"committed\n").unwrap();
        });
        commit_stream(&mut receiver).unwrap();
        peer.join().unwrap();
    }

    #[test]
    fn source_failure_before_commit_is_not_success() {
        let (mut receiver, mut source) = UnixStream::pair().unwrap();
        let peer = std::thread::spawn(move || {
            assert_eq!(source::read_line(&mut source, 128).unwrap(), b"restored");
            assert_eq!(source::read_line(&mut source, 128).unwrap(), b"ready");
        });
        assert!(commit_stream(&mut receiver).is_err());
        peer.join().unwrap();
    }
}
