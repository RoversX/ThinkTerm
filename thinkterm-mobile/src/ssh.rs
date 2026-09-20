//! The network thread: an ssh session to the host, an exec channel running
//! `thinkterm cli --prefer-mux proxy`, and the PDU bytes flowing both ways.
//!
//! One tokio thread per connection. Bytes to send arrive on an unbounded
//! channel and are written in order; everything that comes back -- data,
//! stderr, the exit status, the close -- is delivered to the core thread
//! through the callback it gave us, which must not block.
//!
//! The host key is trusted on first use: its fingerprint is reported so
//! the shell can remember it, and a remembered one that no longer matches
//! ends the dial before any authentication.

use anyhow::{anyhow, Context, Result};
use russh::client::{self, Handler};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::ChannelMsg;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

#[derive(Debug, Clone)]
pub enum Auth {
    /// An OpenSSH private key file on disk (the probe's throwaway key).
    KeyPath(String),
    /// A private key in OpenSSH or PEM text, with its passphrase if any.
    KeyPem {
        pem: String,
        passphrase: Option<String>,
    },
    Password(String),
}

#[derive(Debug, Clone)]
pub struct SshParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: Auth,
    /// The host key fingerprint seen last time (`SHA256:...`), if any.
    pub known_host: Option<String>,
    /// The command to exec; empty means the default proxy invocation.
    pub remote_command: String,
    /// Seconds between keep-alives; 0 sends none.
    pub keepalive_secs: u64,
}

pub enum Out {
    Bytes(Vec<u8>),
    Close,
}

#[derive(Debug)]
pub enum Net {
    /// The host's key fingerprint, before authentication.
    HostKey(String),
    Connected,
    Data(Vec<u8>),
    Stderr(String),
    Exit(u32),
    Closed(String),
}

struct HostKeyCheck {
    known: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

impl Handler for HostKeyCheck {
    type Error = anyhow::Error;
    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool> {
        let fingerprint = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                key.fingerprint(HashAlg::Sha256).to_string()
            }
            PublicKeyOrCertificate::Certificate(cert) => {
                cert.public_key().fingerprint(HashAlg::Sha256).to_string()
            }
        };
        *self.seen.lock().unwrap() = Some(fingerprint.clone());
        match &self.known {
            Some(known) if known != &fingerprint => Err(anyhow!(
                "the host's key changed: expected {known}, got {fingerprint}. If the host was reinstalled, forget it and add it again"
            )),
            _ => Ok(true),
        }
    }
}

/// Asked of the user's login shell: where the host keeps its ThinkTerm.
/// An exec session runs with the daemon's bare PATH (on macOS without
/// /usr/local/bin, where the desktop's CLI lives); the login shell's
/// profile sets the user's own. The usual places come after, for a
/// profile that sets nothing. Prints the path, or exits 127.
const PROBE: &str = "for b in thinkterm wezterm; do \
    p=$(command -v \"$b\" 2>/dev/null) && { printf %s\\\\n \"$p\"; exit 0; }; done; \
    for p in \"$HOME/.local/bin/thinkterm\" /usr/local/bin/thinkterm /opt/homebrew/bin/thinkterm \
    /Applications/ThinkTerm.app/Contents/MacOS/thinkterm; do \
    [ -x \"$p\" ] && { printf %s\\\\n \"$p\"; exit 0; }; done; exit 127";

/// A word for the remote shell, whatever is in it.
fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The CLI's path on the host, from the probe. Only the probe runs in
/// the login shell: a profile may print, and on the channel that
/// carries the protocol that would be corruption. The last line that
/// looks like a path is the answer, whatever came before it.
async fn find_cli(session: &client::Handle<HostKeyCheck>) -> Result<String> {
    let mut probe = bounded("opening the probe channel", session.channel_open_session()).await?;
    let command = format!("exec \"${{SHELL:-/bin/sh}}\" -lc '{PROBE}'");
    bounded("probing for thinkterm", probe.exec(true, command)).await?;
    let (mut out, mut err, mut status) = (Vec::new(), String::new(), None);
    loop {
        match tokio::time::timeout(STEP, probe.wait()).await {
            Ok(Some(ChannelMsg::Data { data })) => out.extend_from_slice(&data),
            Ok(Some(ChannelMsg::ExtendedData { data, .. })) => {
                err.push_str(&String::from_utf8_lossy(&data));
            }
            Ok(Some(ChannelMsg::ExitStatus { exit_status })) => status = Some(exit_status),
            Ok(Some(ChannelMsg::Close)) | Ok(None) => break,
            Ok(Some(_)) => {}
            Err(_) => anyhow::bail!("probing for thinkterm timed out"),
        }
    }
    let text = String::from_utf8_lossy(&out);
    let found = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with('/') && (l.ends_with("/thinkterm") || l.ends_with("/wezterm")));
    match found {
        Some(path) => Ok(path.to_string()),
        None => {
            let err = err.trim();
            let detail = if err.is_empty() {
                match status {
                    Some(127) | None => String::new(),
                    Some(code) => format!(" (the shell exited with {code})"),
                }
            } else {
                format!(": {}", err.lines().last().unwrap_or(""))
            };
            anyhow::bail!(
                "no thinkterm on the host{detail}. Install it there, or set the host's remote command"
            )
        }
    }
}

/// Start the connection on its own thread. Returns the sender for bytes
/// to write; dropping it (or sending `Out::Close`) ends the session.
pub fn spawn(params: SshParams, deliver: Box<dyn Fn(Net) + Send>) -> UnboundedSender<Out> {
    let (out_tx, out_rx) = unbounded_channel();
    std::thread::Builder::new()
        .name("thinkterm-net".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    deliver(Net::Closed(format!("tokio runtime: {err}")));
                    return;
                }
            };
            let outcome = runtime.block_on(run(params, out_rx, &deliver));
            let reason = match outcome {
                Ok(reason) => reason,
                Err(err) => format!("{err:#}"),
            };
            deliver(Net::Closed(reason));
        })
        .expect("spawning the network thread");
    out_tx
}

/// How long any one step of the handshake may take. The transport's
/// own keepalive only starts once the login is through, so until then
/// this is the only thing that ends a server that stops answering.
const STEP: Duration = Duration::from_secs(15);

async fn bounded<T, E>(
    what: &'static str,
    fut: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> Result<T>
where
    E: std::error::Error + Send + Sync + 'static,
{
    match tokio::time::timeout(STEP, fut).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(anyhow::Error::new(err).context(what)),
        Err(_) => Err(anyhow!("{what} timed out")),
    }
}

async fn run(
    params: SshParams,
    mut out_rx: UnboundedReceiver<Out>,
    deliver: &dyn Fn(Net),
) -> Result<String> {
    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: (params.keepalive_secs > 0).then(|| Duration::from_secs(params.keepalive_secs)),
        keepalive_max: 3,
        ..Default::default()
    });
    let seen = Arc::new(Mutex::new(None));
    let handler = HostKeyCheck {
        known: params.known_host.clone(),
        seen: Arc::clone(&seen),
    };
    let dial = tokio::time::timeout(
        STEP,
        client::connect(config, (params.host.as_str(), params.port), handler),
    )
    .await
    .context("connecting timed out")?;
    let mut session = dial.context("ssh transport")?;
    // Only a key the check let through is worth remembering: the shells
    // pin whatever arrives here, and a mismatch must not replace the pin.
    if let Some(fingerprint) = seen.lock().unwrap().clone() {
        deliver(Net::HostKey(fingerprint));
    }

    let auth = match &params.auth {
        Auth::KeyPath(path) => {
            let key = russh::keys::load_secret_key(path, None)
                .with_context(|| format!("loading the key {path}"))?;
            bounded(
                "publickey auth",
                session.authenticate_publickey(
                    &params.user,
                    PrivateKeyWithHashAlg::new(Arc::new(key), None),
                ),
            )
            .await?
        }
        Auth::KeyPem { pem, passphrase } => {
            let key = russh::keys::decode_secret_key(pem, passphrase.as_deref())
                .context("reading the private key")?;
            bounded(
                "publickey auth",
                session.authenticate_publickey(
                    &params.user,
                    PrivateKeyWithHashAlg::new(Arc::new(key), None),
                ),
            )
            .await?
        }
        Auth::Password(password) => bounded(
            "password auth",
            session.authenticate_password(&params.user, password.as_str()),
        )
        .await?,
    };
    if !auth.success() {
        anyhow::bail!("the host refused the login for {}", params.user);
    }

    let mut channel = bounded("opening a session channel", session.channel_open_session()).await?;
    let command = if params.remote_command.trim().is_empty() {
        let cli = find_cli(&session).await?;
        format!("exec {} cli --prefer-mux proxy", shell_quote(&cli))
    } else {
        params.remote_command.clone()
    };
    let command = command.as_str();
    bounded("exec", channel.exec(true, command)).await?;
    deliver(Net::Connected);

    // A write waits for the remote's window, and the window only grows
    // through messages the reader takes in; russh parks its session loop
    // on a full inbound queue. So the writer runs apart from the reader,
    // and a stalled write never stops the reading.
    let (mut reader, writer) = channel.split();
    let mut sender = tokio::spawn(async move {
        while let Some(out) = out_rx.recv().await {
            match out {
                Out::Bytes(bytes) => writer
                    .data(&bytes[..])
                    .await
                    .context("writing to the channel")?,
                Out::Close => break,
            }
        }
        let _ = writer.eof().await;
        let _ = writer.close().await;
        Ok::<(), anyhow::Error>(())
    });
    // What the command last said and how it ended: the reason a session
    // that dies at once gives, instead of a bare "ended".
    let (mut last_err, mut exit) = (String::new(), None);
    loop {
        tokio::select! {
            sent = &mut sender => {
                let _ = session
                    .disconnect(russh::Disconnect::ByApplication, "closed", "")
                    .await;
                return match sent {
                    Ok(Ok(())) => Ok("closed by the client".into()),
                    Ok(Err(err)) => Err(err),
                    Err(err) => Err(anyhow!("the writer ended: {err}")),
                };
            }
            msg = reader.wait() => match msg {
                Some(ChannelMsg::Data { data }) => deliver(Net::Data(data.to_vec())),
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    let text = String::from_utf8_lossy(&data).to_string();
                    if let Some(line) = text.lines().rev().find(|l| !l.trim().is_empty()) {
                        last_err = line.trim().to_string();
                    }
                    deliver(Net::Stderr(text))
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    exit = Some(exit_status);
                    deliver(Net::Exit(exit_status))
                }
                Some(ChannelMsg::Eof) => {}
                Some(ChannelMsg::Close) | None => {
                    sender.abort();
                    let mut reason = String::from("the remote command ended");
                    if let Some(code) = exit.filter(|c| *c != 0) {
                        reason.push_str(&format!(" with status {code}"));
                    }
                    if !last_err.is_empty() {
                        reason.push_str(&format!(": {last_err}"));
                    }
                    return Ok(reason);
                }
                Some(_) => {}
            },
        }
    }
}
