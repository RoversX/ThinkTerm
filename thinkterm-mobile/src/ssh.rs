//! The network thread: an ssh session to the host, an exec channel running
//! `thinkterm cli --prefer-mux proxy`, and the PDU bytes flowing both ways.
//!
//! One tokio thread per connection. Bytes to send arrive on an unbounded
//! channel and are written in order; everything that comes back -- data,
//! stderr, the exit status, the close -- is delivered to the core thread
//! through the callback it gave us, which must not block.

use anyhow::{Context, Result};
use russh::client::{self, Handler};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::ChannelMsg;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

#[derive(Debug, Clone)]
pub struct SshParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Path to an OpenSSH private key file (unencrypted for the probe).
    pub key_path: String,
    /// The command to exec; empty means the default proxy invocation.
    pub remote_command: String,
}

pub enum Out {
    Bytes(Vec<u8>),
    Close,
}

#[derive(Debug)]
pub enum Net {
    Connected,
    Data(Vec<u8>),
    Stderr(String),
    Exit(u32),
    Closed(String),
}

struct Trusting;

impl Handler for Trusting {
    type Error = anyhow::Error;
    async fn check_server_key(&mut self, _key: &PublicKeyOrCertificate) -> Result<bool> {
        // The probe trusts any host key; the real client pins it on first
        // use and refuses a change.
        Ok(true)
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

async fn run(
    params: SshParams,
    mut out_rx: UnboundedReceiver<Out>,
    deliver: &dyn Fn(Net),
) -> Result<String> {
    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        ..Default::default()
    });
    let mut session = tokio::time::timeout(
        Duration::from_secs(15),
        client::connect(config, (params.host.as_str(), params.port), Trusting),
    )
    .await
    .context("connecting timed out")?
    .context("ssh transport")?;

    let key = russh::keys::load_secret_key(&params.key_path, None)
        .with_context(|| format!("loading the key {}", params.key_path))?;
    let auth = session
        .authenticate_publickey(
            &params.user,
            PrivateKeyWithHashAlg::new(Arc::new(key), None),
        )
        .await
        .context("publickey auth")?;
    if !auth.success() {
        anyhow::bail!("the host refused the key for {}", params.user);
    }

    let mut channel = session
        .channel_open_session()
        .await
        .context("opening a session channel")?;
    let command = if params.remote_command.trim().is_empty() {
        DEFAULT_COMMAND
    } else {
        params.remote_command.as_str()
    };
    channel.exec(true, command).await.context("exec")?;
    deliver(Net::Connected);

    loop {
        tokio::select! {
            out = out_rx.recv() => match out {
                Some(Out::Bytes(bytes)) => {
                    channel.data(&bytes[..]).await.context("writing to the channel")?;
                }
                Some(Out::Close) | None => {
                    let _ = channel.eof().await;
                    let _ = channel.close().await;
                    let _ = session
                        .disconnect(russh::Disconnect::ByApplication, "closed", "")
                        .await;
                    return Ok("closed by the client".into());
                }
            },
            msg = channel.wait() => match msg {
                Some(ChannelMsg::Data { data }) => deliver(Net::Data(data.to_vec())),
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    deliver(Net::Stderr(String::from_utf8_lossy(&data).to_string()))
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => deliver(Net::Exit(exit_status)),
                Some(ChannelMsg::Eof) => {}
                Some(ChannelMsg::Close) | None => return Ok("the remote command ended".into()),
                Some(_) => {}
            },
        }
    }
}
