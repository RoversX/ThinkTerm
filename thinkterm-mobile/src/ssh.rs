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

const DEFAULT_COMMAND: &str = "if [ -x \"$HOME/.local/bin/thinkterm\" ]; \
    then exec \"$HOME/.local/bin/thinkterm\" cli --prefer-mux proxy; \
    elif command -v thinkterm >/dev/null 2>&1; \
    then exec thinkterm cli --prefer-mux proxy; else exec wezterm cli --prefer-mux proxy; fi";

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
        DEFAULT_COMMAND
    } else {
        params.remote_command.as_str()
    };
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
                    deliver(Net::Stderr(String::from_utf8_lossy(&data).to_string()))
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => deliver(Net::Exit(exit_status)),
                Some(ChannelMsg::Eof) => {}
                Some(ChannelMsg::Close) | None => {
                    sender.abort();
                    return Ok("the remote command ended".into());
                }
                Some(_) => {}
            },
        }
    }
}
