//! The network thread: an ssh session to the host, an exec channel running
//! `thinkterm cli --prefer-mux proxy`, and the PDU bytes flowing both ways.
//! Beside it, once a connection, the host desktop's tab icons are read on
//! channels of their own (`icons`).
//!
//! One tokio thread per connection. Bytes to send arrive on an unbounded
//! channel and are written in order; everything that comes back -- data,
//! stderr, the exit status, the close -- is delivered to the core thread
//! through the callback it gave us, which must not block.
//!
//! An unknown host key ends the transport before authentication. The shell
//! asks for explicit approval and retries with that exact fingerprint pinned.
//! A remembered key that changes is always rejected.

use crate::icons;
use anyhow::{anyhow, Context, Result};
use russh::client::{self, Handler};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::ChannelMsg;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thinkterm_tab_icons::WireCatalog;
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
    pub request_id: String,
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
    /// A file of this device's to leave on the host, beside the protocol
    /// (`upload`): read as it is sent, never held whole.
    Upload {
        id: u64,
        name: String,
        path: String,
    },
    /// Give upload `id` up: what reached the host of it is removed there.
    CancelUpload(u64),
    /// See whether the connection still carries anything (`PROBE_WAIT`):
    /// one that does not is ended at once, rather than when the
    /// keep-alives have gone unanswered long enough.
    Probe,
    Close,
}

#[derive(Debug)]
pub enum Net {
    /// Transport stopped before authentication; only explicit approval may retry.
    HostKeyRequired {
        request_id: String,
        fingerprint: String,
    },
    Connected,
    Data(Vec<u8>),
    Stderr(String),
    Exit(u32),
    /// The host's tab icon cards, read beside the protocol, or why not.
    TabIcons(Result<WireCatalog, String>),
    /// How much of upload `id` has gone, of how much.
    UploadProgress {
        id: u64,
        sent: u64,
        total: u64,
    },
    /// Where upload `id` landed on the host, or why it did not.
    Uploaded {
        id: u64,
        result: Result<String, String>,
    },
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
            Some(_) => Ok(true),
            None => Err(anyhow!("host key approval required before authentication")),
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

/// The longest a command beside the protocol may take, all told.
const OUTPUT_DEADLINE: Duration = Duration::from_secs(60);

/// What `command` prints, on a channel of its own beside the protocol's;
/// more than `limit` bytes is an error. A command that runs on (a forced
/// command in place of the shell, a profile that waits) is ended with its
/// channel.
async fn output(session: &client::Handle<HostKeyCheck>, command: String, limit: usize) -> Result<Vec<u8>> {
    let mut channel = bounded("opening a channel", session.channel_open_session()).await?;
    let mut out = Vec::new();
    let read = async {
        bounded("exec", channel.exec(true, command)).await?;
        loop {
            match tokio::time::timeout(STEP, channel.wait()).await {
                Ok(Some(ChannelMsg::Data { data })) => {
                    out.extend_from_slice(&data);
                    anyhow::ensure!(out.len() <= limit, "more than {limit} bytes");
                }
                Ok(Some(ChannelMsg::Close)) | Ok(None) => return Ok::<(), anyhow::Error>(()),
                Ok(Some(_)) => {}
                Err(_) => anyhow::bail!("no answer"),
            }
        }
    };
    // Each message re-arms the step's timeout; this bounds them all.
    let read = match tokio::time::timeout(OUTPUT_DEADLINE, read).await {
        Ok(read) => read,
        Err(_) => Err(anyhow!("no end in {} s", OUTPUT_DEADLINE.as_secs())),
    };
    if read.is_err() {
        let _ = channel.close().await;
    }
    read.map(|()| out)
}

/// Where the desktop leaves what is dropped on a remote terminal, under
/// the login's home (`native_settings::DEFAULT_REMOTE_DROP_DESTINATION`).
const UPLOADS: &str = "ThinkTerm_Uploads";

/// The line the upload's answer is on: whatever a profile says comes
/// before it.
const UPLOADED: &str = "thinkterm-upload ";

/// The longest an upload may take, all told: a large file over a slow
/// line. One that hangs is given up by whoever watches its progress.
const UPLOAD_DEADLINE: Duration = Duration::from_secs(6 * 3600);

/// A name for a file on the host: its last component, nothing a shell or
/// a listing would trip on, and never none.
pub fn upload_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = last.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim().trim_start_matches('.').to_string();
    match clean.is_empty() {
        true => "file".to_string(),
        false => clean,
    }
}

/// Leaves what it reads in the uploads folder under `$1` -- or under
/// that name with ` (1)`, ` (2)`... before its extension where one is
/// there already, the desktop's collision names -- and prints where. It
/// is written beside its place first and moved there only once all `$2`
/// bytes of it have come: an upload given up, or cut off with its
/// connection, leaves nothing behind. The name is claimed by creating it
/// (`set -C`, which fails on one that is there), so two uploads of one
/// name ending together never both take it. `/bin/sh` runs it, whatever
/// the login shell is.
fn upload_command(name: &str, size: u64) -> String {
    format!(
        r#"/bin/sh -c 'd="$HOME/{UPLOADS}"; mkdir -p "$d" || exit 1; n=$1; t="$d/.thinkterm-upload.$$"; cat > "$t" || {{ rm -f "$t"; exit 1; }}; if [ "$(wc -c < "$t" | tr -d " ")" != "$2" ]; then rm -f "$t"; exit 1; fi; b=${{n%.*}}; e=${{n##*.}}; if [ "$b" = "$n" ]; then e=; else e=.$e; fi; p="$d/$n"; i=1; while :; do if [ ! -e "$p" ] && [ ! -L "$p" ]; then (set -C; : > "$p") 2>/dev/null && break; [ -e "$p" ] || [ -L "$p" ] || {{ rm -f "$t"; exit 1; }}; fi; p="$d/$b ($i)$e"; i=$((i+1)); done; mv -f "$t" "$p" || {{ rm -f "$t" "$p"; exit 1; }}; printf "\n{UPLOADED}%s\n" "$p"' sh {} {size}"#,
        shell_quote(name)
    )
}

/// Where the upload's answer says the file is.
fn uploaded_path(out: &[u8]) -> Option<String> {
    String::from_utf8_lossy(out)
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(UPLOADED))
        .filter(|path| path.starts_with('/'))
        .map(str::to_string)
}

/// How much of a file goes in one message: small enough that an upload
/// given up stops soon and its progress moves smoothly.
const UPLOAD_CHUNK: usize = 32 * 1024;

/// The most of the host's own words an upload keeps: its answer is the
/// last line, and whatever a login profile prints comes before it.
const UPLOAD_SAID: usize = 64 * 1024;

/// How long a connection has to show it still carries anything when asked
/// (`Out::Probe`): long enough for a slow line, short of what a person
/// waits before they kill the app.
const PROBE_WAIT: Duration = Duration::from_secs(5);

/// A probe's channel is asked for behind whatever the session has queued,
/// and uploads queue a lot: on a slow line its answer comes long after
/// `PROBE_WAIT`. While an upload is under way and the host took more of
/// one in this recently -- counted from the probe's asking, so a phone that
/// slept with the connection dead is not fooled by what moved before --
/// the probe waits on. A host says it took more by widening the upload's
/// window, half the window at a time (a megabyte, for OpenSSH): long
/// enough for the first such step of three uploads started together on a
/// line of a megabit a second, and no longer, for a connection that died.
const UPLOAD_STILL: Duration = Duration::from_secs(30);

/// How many uploads go at once; the rest wait their turn. Each is a
/// channel of the session's, and a host allows only so many (OpenSSH's
/// `MaxSessions` is 10, the terminal's own among them).
const UPLOADS_AT_ONCE: usize = 3;

/// Resolves once `cancel` says the upload was given up; never, if nobody
/// is left to say so.
async fn given_up(cancel: &tokio::sync::watch::Receiver<bool>) {
    let mut cancel = cancel.clone();
    if cancel.wait_for(|given_up| *given_up).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Leave the file at `path` on the host as `name`, on a channel of its
/// own beside the protocol's, and say where it is. It is read as it goes;
/// `progress` hears how much has gone, `took` each time the host says it
/// took more in, and `cancel` set ends it -- at once, wherever the upload
/// is, and a file that had already landed is removed again.
async fn upload(
    session: &client::Handle<HostKeyCheck>,
    name: &str,
    path: &str,
    cancel: &tokio::sync::watch::Receiver<bool>,
    progress: &dyn Fn(u64, u64),
    took: &dyn Fn(),
) -> Result<String> {
    use std::io::Read as _;
    let cancelled = || *cancel.borrow();
    let mut file = std::fs::File::open(path).context("opening the file")?;
    let total = file.metadata().context("reading the file's size")?.len();
    let channel = bounded("opening a channel", session.channel_open_session()).await?;
    let command = upload_command(&upload_name(name), total);
    let send = async {
        bounded("exec", channel.exec(true, command)).await?;
        // As the protocol's channel: the writer waits for the remote's
        // window, which grows only as the reader takes messages in.
        let (mut reader, writer) = channel.split();
        let write = async {
            let mut chunk = vec![0u8; UPLOAD_CHUNK];
            let mut sent = 0u64;
            progress(0, total);
            loop {
                anyhow::ensure!(!cancelled(), "cancelled");
                let read = file.read(&mut chunk).context("reading the file")?;
                if read == 0 {
                    break;
                }
                // A write may wait long for the host's window on a slow
                // line; giving up does not wait with it.
                tokio::select! {
                    biased;
                    () = given_up(cancel) => anyhow::bail!("cancelled"),
                    written = writer.data(&chunk[..read]) => written.context("writing the file")?,
                }
                sent += read as u64;
                progress(sent, total);
            }
            Ok::<(), anyhow::Error>(())
        };
        // Its end is said whether it went whole or not: the host counts
        // what came and keeps nothing of a file that came short.
        let write = async {
            let written = write.await;
            let _ = writer.eof().await;
            written
        };
        // Once given up, the host has a while to clear away what came and
        // say so; a host that does not is left to its channel's close.
        let gave_up = async {
            given_up(cancel).await;
            tokio::time::sleep(STEP).await;
        };
        let read = async {
            tokio::pin!(gave_up);
            let (mut out, mut err, mut status) = (Vec::new(), String::new(), None);
            loop {
                let msg = tokio::select! {
                    msg = reader.wait() => msg,
                    () = &mut gave_up => break,
                };
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        // Read to the end whatever comes: a channel nobody
                        // reads fills, and a full one stalls the session's
                        // every channel, the terminal's too. Only the last
                        // of it is kept; the answer is its last line.
                        out.extend_from_slice(&data);
                        if out.len() > UPLOAD_SAID {
                            out.drain(..out.len() - UPLOAD_SAID);
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => err = String::from_utf8_lossy(&data).to_string(),
                    Some(ChannelMsg::ExitStatus { exit_status }) => status = Some(exit_status),
                    // The host took more of the file in: the connection
                    // carries, however slowly.
                    Some(ChannelMsg::WindowAdjusted { .. }) => took(),
                    Some(ChannelMsg::Close) | None => break,
                    Some(_) => {}
                }
            }
            match uploaded_path(&out) {
                Some(path) if status.unwrap_or(0) == 0 => Ok(path),
                _ => {
                    let said = err.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
                    match said.is_empty() {
                        true => Err(anyhow!("the host did not take the file")),
                        false => Err(anyhow!("{said}")),
                    }
                }
            }
        };
        let (written, read) = tokio::join!(write, read);
        if cancelled() {
            let _ = writer.close().await;
            // Given up after the last of it went: the host may have put
            // the file in its place already, and it is taken away again.
            if let Ok(landed) = &read {
                let _ = output(session, format!("rm -f -- {}", shell_quote(landed)), UPLOAD_SAID).await;
            }
            anyhow::bail!("cancelled");
        }
        // Why the sending stopped is the reason, before what the host
        // made of a file that came short.
        match (written, read) {
            (Err(err), _) => Err(err),
            (Ok(()), read) => read,
        }
    };
    match tokio::time::timeout(UPLOAD_DEADLINE, send).await {
        Ok(sent) => sent,
        Err(_) => Err(anyhow!("no end in {} s", UPLOAD_DEADLINE.as_secs())),
    }
}

/// The host desktop's tab icon cards, as its server gives a page them
/// (`icons`): its settings, then the glyphs it imported that they name.
async fn tab_icons(session: &client::Handle<HostKeyCheck>) -> Result<WireCatalog> {
    // Room for what a login profile prints before the file.
    let out = output(session, icons::settings_command(), icons::SETTINGS_LIMIT + 64 * 1024).await?;
    let cards = icons::desktop_cards(&out).ok_or_else(|| anyhow!("no answer from /bin/sh on the host"))?;
    let hashes = icons::imported(&cards);
    // Glyphs that cannot be read are left out and the cards kept, as the
    // host's server sends them to a page.
    let svgs = match hashes.is_empty() {
        true => Default::default(),
        false => output(session, icons::svgs_command(&hashes), icons::svgs_limit(hashes.len()))
            .await
            .map(|out| icons::imported_svgs(&out, &hashes))
            .unwrap_or_default(),
    };
    Ok(cards.wire(|hash| svgs.get(hash).cloned()))
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
    // A machine of the tailnet the app has joined is reached through the
    // app's own node; every other host by the system's network.
    let dial = match crate::tailnet::dial(&params.host, params.port).await {
        Some(Ok(stream)) => tokio::time::timeout(STEP, client::connect_stream(config, stream, handler)).await,
        Some(Err(err)) => return Err(err.context("reaching the machine through the tailnet")),
        None => {
            tokio::time::timeout(STEP, client::connect(config, (params.host.as_str(), params.port), handler)).await
        }
    }
    .context("connecting timed out")?;
    if dial.is_err() && params.known_host.is_none() {
        if let Some(fingerprint) = seen.lock().unwrap().clone() {
            deliver(Net::HostKeyRequired {
                request_id: params.request_id.clone(),
                fingerprint,
            });
        }
    }
    let mut session = dial.context("ssh transport")?;

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
    // Files go on channels of their own, opened here where the session is.
    let (upload_tx, mut upload_rx) = unbounded_channel::<Out>();
    let mut sender = tokio::spawn(async move {
        while let Some(out) = out_rx.recv().await {
            match out {
                Out::Bytes(bytes) => writer
                    .data(&bytes[..])
                    .await
                    .context("writing to the channel")?,
                beside @ (Out::Upload { .. } | Out::CancelUpload(_) | Out::Probe) => {
                    let _ = upload_tx.send(beside);
                }
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
    // The host's tab icons come on channels of their own while the
    // terminal is already in use.
    let cards = tab_icons(&session);
    tokio::pin!(cards);
    let mut cards_read = false;
    // How many uploads are under way, and when the host last took more of
    // one in: a probe stuck behind their data is no sign of a dead
    // connection while they go on moving (`UPLOAD_STILL`).
    let going = std::cell::Cell::new(0usize);
    let moved = std::cell::Cell::new(None::<std::time::Instant>);
    // Uploads under way, each on its channel while the terminal goes on,
    // and those waiting their turn.
    let mut uploads = futures::stream::FuturesUnordered::new();
    let mut waiting: std::collections::VecDeque<(u64, String, String)> = Default::default();
    // Each one's word that it was given up, until it ends.
    let mut cancels: std::collections::HashMap<u64, tokio::sync::watch::Sender<bool>> = Default::default();
    // A look at whether the connection still carries anything: a channel
    // opened and closed again, which needs the host's answer.
    let mut probes = futures::stream::FuturesUnordered::new();
    let start = |id: u64, name: String, path: String, cancels: &mut std::collections::HashMap<u64, tokio::sync::watch::Sender<bool>>| {
        let (cancel_tx, cancel) = tokio::sync::watch::channel(false);
        cancels.insert(id, cancel_tx);
        let (session, moved) = (&session, &moved);
        async move {
            // Told at each hundredth, not at each message.
            let last = std::cell::Cell::new(u64::MAX);
            let progress = |sent: u64, total: u64| {
                let step = if total == 0 { 100 } else { sent * 100 / total };
                if last.replace(step) != step {
                    deliver(Net::UploadProgress { id, sent, total });
                }
            };
            let took = || moved.set(Some(std::time::Instant::now()));
            let result = upload(session, &name, &path, &cancel, &progress, &took).await;
            (id, result.map_err(|err| format!("{err:#}")))
        }
    };
    loop {
        tokio::select! {
            read = &mut cards, if !cards_read => {
                cards_read = true;
                deliver(Net::TabIcons(read.map_err(|err| format!("{err:#}"))));
            }
            Some(asked) = upload_rx.recv() => match asked {
                Out::Upload { id, name, path } => {
                    if uploads.len() < UPLOADS_AT_ONCE {
                        uploads.push(start(id, name, path, &mut cancels));
                        going.set(uploads.len());
                    } else {
                        waiting.push_back((id, name, path));
                    }
                }
                Out::Probe => {
                    if probes.is_empty() {
                        let (session, going, moved) = (&session, &going, &moved);
                        probes.push(async move {
                            let asked = std::time::Instant::now();
                            let open = session.channel_open_session();
                            tokio::pin!(open);
                            loop {
                                match tokio::time::timeout(PROBE_WAIT, &mut open).await {
                                    Ok(Ok(channel)) => {
                                        let _ = channel.close().await;
                                        return true;
                                    }
                                    // Turned down -- the host allows only so
                                    // many channels -- but answered: the
                                    // connection carries.
                                    Ok(Err(russh::Error::ChannelOpenFailure(_))) => return true,
                                    Ok(Err(_)) => return false,
                                    // No answer yet: behind uploads that go
                                    // on moving, it is waited for.
                                    Err(_) => {
                                        let since = moved.get().map_or(asked, |at| at.max(asked));
                                        if going.get() == 0 || since.elapsed() >= UPLOAD_STILL {
                                            return false;
                                        }
                                    }
                                }
                            }
                        });
                    }
                }
                Out::CancelUpload(id) => {
                    if let Some(at) = waiting.iter().position(|(waiting, ..)| *waiting == id) {
                        waiting.remove(at);
                        deliver(Net::Uploaded { id, result: Err("cancelled".into()) });
                    } else if let Some(cancel) = cancels.get(&id) {
                        let _ = cancel.send(true);
                    }
                }
                _ => {}
            },
            Some(alive) = futures::StreamExt::next(&mut probes), if !probes.is_empty() => {
                if !alive {
                    // Gone without a word, as a connection is that a
                    // network change or a long sleep cut off.
                    sender.abort();
                    return Ok("the connection dropped".into());
                }
            }
            Some((id, result)) = futures::StreamExt::next(&mut uploads), if !uploads.is_empty() => {
                cancels.remove(&id);
                deliver(Net::Uploaded { id, result });
                if let Some((id, name, path)) = waiting.pop_front() {
                    uploads.push(start(id, name, path, &mut cancels));
                }
                going.set(uploads.len());
            }
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
                // The session itself ended under the channel, with no
                // word from the host: the connection was cut.
                None if exit.is_none() => {
                    sender.abort();
                    return Ok("the connection dropped".into());
                }
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

#[cfg(test)]
mod host_key_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct PasswordServer(Arc<AtomicUsize>);
    impl russh::server::Handler for PasswordServer {
        type Error = anyhow::Error;
        async fn auth_password(&mut self, _: &str, password: &str) -> Result<russh::server::Auth> {
            assert_eq!(password, "test-secret");
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(russh::server::Auth::Accept)
        }
    }

    #[tokio::test]
    async fn unknown_and_changed_keys_never_receive_credentials() {
        let key = russh::keys::PrivateKey::random(
            &mut crate::SysRng(ring::rand::SystemRandom::new()),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
        let config = Arc::new(russh::server::Config {
            keys: vec![key],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        // The same real SSH transport is exercised with no pin, a mismatched
        // pin and the explicitly approved pin. No UI or transport mock.
        for (known_host, expected_auth, expected_challenge) in [
            (None, 0, true),
            (Some("SHA256:wrong".to_string()), 0, false),
            (Some(fingerprint.clone()), 1, false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let count = Arc::new(AtomicUsize::new(0));
            let server_count = count.clone();
            let server_config = config.clone();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                if let Ok(session) =
                    russh::server::run_stream(server_config, stream, PasswordServer(server_count))
                        .await
                {
                    let _ = session.await;
                }
            });
            let (_sender, receiver) = unbounded_channel();
            let events = std::cell::RefCell::new(Vec::new());
            let params = SshParams {
                request_id: "attempt-42".into(),
                host: "127.0.0.1".into(),
                port,
                user: "test".into(),
                auth: Auth::Password("test-secret".into()),
                known_host,
                remote_command: "test".into(),
                keepalive_secs: 0,
            };
            // The server intentionally refuses opening a session after auth.
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                run(params, receiver, &|event| events.borrow_mut().push(event)),
            )
            .await;
            assert!(result
                .expect("transport completes within the test deadline")
                .is_err());
            assert_eq!(count.load(Ordering::SeqCst), expected_auth);
            let challenges: Vec<_> = events
                .borrow()
                .iter()
                .filter_map(|event| match event {
                    Net::HostKeyRequired {
                        request_id,
                        fingerprint,
                    } => Some((request_id.clone(), fingerprint.clone())),
                    _ => None,
                })
                .collect();
            assert_eq!(
                challenges,
                if expected_challenge {
                    vec![("attempt-42".into(), fingerprint.clone())]
                } else {
                    vec![]
                }
            );
            server.abort();
            let _ = server.await;
        }
    }
}

#[cfg(test)]
mod upload_tests {
    use super::*;

    #[test]
    fn an_upload_s_name_is_safe_to_leave_on_a_host() {
        assert_eq!(upload_name("Pasted 2026-01-02 at 03.04.05.png"), "Pasted 2026-01-02 at 03.04.05.png");
        assert_eq!(upload_name("/private/var/tmp/a b.txt"), "a b.txt");
        assert_eq!(upload_name("..\\..\\evil"), "evil");
        assert_eq!(upload_name(".hidden"), "hidden");
        assert_eq!(upload_name("line\nbreak"), "linebreak");
        assert_eq!(upload_name("  "), "file");
        assert_eq!(upload_name("../"), "file");
        // Whatever the name, the command is one shell word for the script
        // and one for the name.
        let command = upload_command("it's (1).png", 42);
        assert!(command.starts_with("/bin/sh -c '") && command.ends_with(r"sh 'it'\''s (1).png' 42"), "{command}");
    }

    #[test]
    fn the_host_says_where_an_upload_landed() {
        assert_eq!(
            uploaded_path(b"Welcome!\n\nthinkterm-upload /home/user/ThinkTerm_Uploads/a (1).png\n"),
            Some("/home/user/ThinkTerm_Uploads/a (1).png".to_string())
        );
        assert_eq!(uploaded_path(b"sh: cat: write error\n"), None);
        assert_eq!(uploaded_path(b"thinkterm-upload relative\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_upload_command_keeps_both_of_two_files_with_one_name() {
        use std::io::Write as _;
        let home = tempfile::tempdir().unwrap();
        let send = |name: &str, bytes: &[u8], size: u64| {
            let mut child = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(upload_command(name, size))
                .env("HOME", home.path())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("running /bin/sh");
            child.stdin.take().unwrap().write_all(bytes).unwrap();
            let out = child.wait_with_output().unwrap();
            uploaded_path(&out.stdout)
        };
        let run = |name: &str, bytes: &[u8]| send(name, bytes, bytes.len() as u64).expect("a path");
        let dir = home.path().join(UPLOADS);
        let first = run("shot 1.png", b"one");
        assert_eq!(first, dir.join("shot 1.png").to_string_lossy());
        let second = run("shot 1.png", b"two");
        assert_eq!(second, dir.join("shot 1 (1).png").to_string_lossy());
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert_eq!(std::fs::read(&second).unwrap(), b"two");
        // No extension, and a quote in the name.
        assert_eq!(run("it's", b"x"), dir.join("it's").to_string_lossy());
        assert_eq!(run("it's", b"y"), dir.join("it's (1)").to_string_lossy());
        // One that came short -- given up, or cut off -- leaves nothing.
        let before = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(send("half.bin", b"12345", 10), None);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), before, "nothing of it is kept");
    }

    #[cfg(unix)]
    #[test]
    fn uploads_of_one_name_ending_together_each_keep_their_own() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        // The shells a host's /bin/sh may be, where this machine has them.
        for shell in ["/bin/sh", "/bin/dash", "/bin/bash"] {
            if !std::path::Path::new(shell).exists() {
                continue;
            }
            let home = tempfile::tempdir().unwrap();
            let command = upload_command("same.txt", 2).replacen("/bin/sh -c", &format!("{shell} -c"), 1);
            let mut children = Vec::new();
            let mut inputs = Vec::new();
            for i in 0..16 {
                let mut child = Command::new("/bin/sh")
                    .arg("-c")
                    .arg(&command)
                    .env("HOME", home.path())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .expect("running the shell");
                let mut input = child.stdin.take().unwrap();
                input.write_all(format!("{i:02}").as_bytes()).unwrap();
                inputs.push(input);
                children.push((i, child));
            }
            // Every one has its bytes; their ends come all at once.
            drop(inputs);
            let mut landed = std::collections::HashSet::new();
            for (i, child) in children {
                let out = child.wait_with_output().unwrap();
                let path = uploaded_path(&out.stdout).unwrap_or_else(|| panic!("{shell}: upload {i} landed nowhere"));
                assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{i:02}"), "{shell}: {path}");
                assert!(landed.insert(path.clone()), "{shell}: two uploads took {path}");
            }
            let kept = std::fs::read_dir(home.path().join(UPLOADS)).unwrap().count();
            assert_eq!(kept, 16, "{shell}: one file each, nothing else left");
        }
    }
}
