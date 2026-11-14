//! A way to a mux server for plugin frames alone.
//!
//! A plugin's page in a pane on another machine is drawn by that machine's
//! plugin, whose frames come through its mux server (`PluginFrame`, which
//! the server carries to and from the plugin host unread). They do not
//! share the connection the terminals are on: a page that draws a lot would
//! sit in the same queue as what is typed and what the terminals print. A
//! link is a second connection to the same server, made the way the first
//! was -- another channel of the SSH session it runs over, another
//! connection to its socket, another run of the command that reaches it,
//! another TLS connection -- with nothing on it but those frames. To the
//! server it is one more client, and a client is sent what every pane
//! prints unless it says it wants to hear only what happens
//! (`SubscribeEvents`): a link says so before anything else, and what
//! little the server still tells it -- a bell, a pane closing -- it lets
//! pass.
//!
//! A link never reconnects by itself. When it ends, whoever holds it is
//! told once, and opens another when it wants one: by then the connection
//! it is opened beside may be a new one too. One whose connection went
//! without a word -- the machine slept, the network changed under it --
//! ends as the first connection does: the server is asked whether it is
//! there after a while of hearing nothing, and is given as long again to
//! say so.

use crate::client::{paired_stream, tls_stream, unix_connect_with_retry, AsyncReadAndWrite, TlsWay};
use crate::connection_io::{with_progress, write_pdu, Counted, IO_STALL_LIMIT};
use codec::{Ping, Pdu, PluginFrame, Pong, SubscribeEvents};
use config::UnixTarget;
use portable_pty::ChildKiller;
use smol::channel::{unbounded, Receiver, Sender};
use smol::Async;
use std::io::Read;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

/// How long nothing is heard from the server before it is asked whether it
/// is there, and how long it then has to answer: the first connection's
/// own (`client_thread_async`), so the two give up on a dead line together.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// How a link to the server a client is connected to is opened, as of the
/// client's last connecting: kept beside the connection, and replaced with
/// it.
#[derive(Clone)]
pub enum PluginWay {
    /// Another channel of the SSH session, running the proxy `command`.
    Ssh {
        session: wezterm_ssh::Session,
        command: String,
    },
    /// Another connection to the server's socket.
    Unix { path: std::path::PathBuf },
    /// Another run of the command the server is reached through.
    Command { argv: Vec<String> },
    /// Another TLS connection.
    Tls(TlsWay),
}

impl std::fmt::Debug for PluginWay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ssh { .. } => f.write_str("PluginWay::Ssh(..)"),
            Self::Unix { path } => write!(f, "PluginWay::Unix({})", path.display()),
            Self::Command { .. } => f.write_str("PluginWay::Command(..)"),
            Self::Tls(_) => f.write_str("PluginWay::Tls(..)"),
        }
    }
}

/// What a link's holder hears, on the link's own thread.
pub enum Heard {
    /// A frame from the plugin host on the server's machine.
    Frame(Vec<u8>),
    /// The link ended, and why: said once, and nothing after it.
    Ended(String),
}

/// A link on its way to being open, or open. Dropping it ends it.
pub struct PluginLink {
    out: Sender<Out>,
}

/// What the link is given to do, in order.
enum Out {
    /// A frame for the plugin host.
    Frame(Vec<u8>),
    /// The holder let go.
    LetGo,
}

impl Drop for PluginLink {
    fn drop(&mut self) {
        // After what was sent before it.
        let _ = self.out.try_send(Out::LetGo);
    }
}

impl PluginLink {
    /// Opens a link by `way`, off the calling thread: frames sent meanwhile
    /// wait, and go in order once it is open. `heard` is told each frame
    /// that comes, and once when the link ends -- which it does straight
    /// away when it cannot be opened -- always on the link's thread, never
    /// before this returns into the caller's hands. Nothing is said of a
    /// link its holder let go. An error is the link's thread not starting:
    /// `heard` is then told nothing.
    pub fn open(
        way: PluginWay,
        heard: impl Fn(Heard) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        Self::open_asking(way, KEEPALIVE_INTERVAL, heard)
    }

    /// [`open`](Self::open), asking the server whether it is there after
    /// `quiet` of hearing nothing from it.
    fn open_asking(
        way: PluginWay,
        quiet: Duration,
        heard: impl Fn(Heard) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let (out, queued) = unbounded::<Out>();
        std::thread::Builder::new()
            .name("plugin-link".into())
            .spawn(move || {
                let ended = match connect(way) {
                    Ok((stream, hang_up)) => {
                        let ended = smol::block_on(carry(stream, &queued, quiet, &heard));
                        // The server lets its way to the plugin host go
                        // with the connection.
                        hang_up();
                        ended
                    }
                    Err(err) => Some(format!("{err:#}")),
                };
                if let Some(why) = ended {
                    heard(Heard::Ended(why));
                }
            })?;
        Ok(Self { out })
    }

    /// Sends `data`, a frame for the plugin host, after those sent before
    /// it. False once the link has ended.
    pub fn send(&self, data: Vec<u8>) -> bool {
        self.out.try_send(Out::Frame(data)).is_ok()
    }
}

/// What a connection is closed with, once its stream is dropped: nothing,
/// for most.
type HangUp = Box<dyn FnOnce() + Send>;

/// The connection a link runs over.
fn connect(way: PluginWay) -> anyhow::Result<(Box<dyn AsyncReadAndWrite>, HangUp)> {
    match way {
        PluginWay::Ssh { session, command } => {
            let exec = smol::block_on(session.exec(&command, None))?;
            let mut stderr = exec.stderr;
            std::thread::spawn(move || {
                let mut said = String::new();
                let _ = stderr.read_to_string(&mut said);
                let said = said.trim();
                if !said.is_empty() {
                    log::warn!("plugin link: {said}");
                }
            });
            let mut child = exec.child;
            let hang_up = Box::new(move || {
                // Its input closed with the stream; this is for one that
                // does not go by itself.
                let _ = child.kill();
            });
            Ok((paired_stream(exec.stdin, exec.stdout)?, hang_up))
        }
        PluginWay::Unix { path } => {
            let stream = unix_connect_with_retry(&UnixTarget::Socket(path), false, Some(1))?;
            Ok((Box::new(Async::new(stream)?), Box::new(|| {})))
        }
        PluginWay::Command { argv } => {
            let program = argv
                .first()
                .ok_or_else(|| anyhow::anyhow!("the proxy command is empty"))?;
            // As the first connection runs it: a socket for what it reads
            // and writes, and its complaints where this program's go.
            let (ours, theirs) = filedescriptor::socketpair()?;
            let mut child = std::process::Command::new(program)
                .args(&argv[1..])
                .stdin(theirs.as_stdio()?)
                .stdout(theirs.as_stdio()?)
                .stderr(std::process::Stdio::inherit())
                .spawn()?;
            drop(theirs);
            let hang_up = Box::new(move || {
                // It goes when its input closes; one that does not is
                // made to, and either way it is not left a zombie.
                let _ = child.kill();
                let _ = child.wait();
            });
            Ok((paired_stream(ours.try_clone()?, ours)?, hang_up))
        }
        PluginWay::Tls(way) => Ok((tls_stream(&way, &mut |_| {})?, Box::new(|| {}))),
    }
}

/// Carries frames both ways until the holder lets go -- `None` -- or the
/// connection fails, has nothing more to carry, or its server stays silent
/// when asked, after `quiet` of nothing, whether it is there -- and why.
async fn carry(
    mut stream: Box<dyn AsyncReadAndWrite>,
    queued: &Receiver<Out>,
    quiet: Duration,
    heard: &(dyn Fn(Heard) + Send + Sync),
) -> Option<String> {
    enum Turn {
        Out(Option<Out>),
        In(anyhow::Result<()>),
        Quiet,
    }
    let failed = |err: anyhow::Error| Some(format!("{err:#}"));
    // No pane's contents are to come this way: said first, or the
    // terminals' output would be sent twice over.
    let mut serial = 1u64;
    if let Err(err) = write_pdu(&mut *stream, &Pdu::SubscribeEvents(SubscribeEvents {}), serial).await {
        return failed(err);
    }
    // Counted from the last thing heard, not the last thing sent: a holder
    // that keeps sending into a dead line would otherwise never ask.
    let mut ask_at = Instant::now() + quiet;
    let mut asked: Option<u64> = None;
    loop {
        let turn = smol::future::or(
            smol::future::or(async { Turn::Out(queued.recv().await.ok()) }, async {
                Turn::In(stream.wait_for_readable().await)
            }),
            async {
                smol::Timer::at(ask_at).await;
                Turn::Quiet
            },
        )
        .await;
        match turn {
            Turn::Quiet => {
                if asked.is_some() {
                    return Some("the server stopped answering".into());
                }
                serial += 1;
                asked = Some(serial);
                ask_at = Instant::now() + quiet;
                if let Err(err) = write_pdu(&mut *stream, &Pdu::Ping(Ping {}), serial).await {
                    return failed(err);
                }
            }
            Turn::Out(Some(Out::Frame(data))) => {
                serial += 1;
                let frame = Pdu::PluginFrame(PluginFrame { data });
                if let Err(err) = write_pdu(&mut *stream, &frame, serial).await {
                    return failed(err);
                }
            }
            Turn::Out(Some(Out::LetGo) | None) => return None,
            Turn::In(Err(err)) => return failed(err),
            Turn::In(Ok(())) => {
                let bytes = AtomicU64::new(0);
                let mut counted = Counted {
                    stream: &mut *stream,
                    bytes: &bytes,
                };
                let read = with_progress(&bytes, IO_STALL_LIMIT, Pdu::decode_async(&mut counted, None)).await;
                let decoded = match read {
                    Ok(decoded) => decoded,
                    Err(err) => return failed(err),
                };
                // What it sends puts the next asking off, but not the
                // answer to one asked already: only that answer does.
                if asked.is_none() || asked == Some(decoded.serial) {
                    asked = None;
                    ask_at = Instant::now() + quiet;
                }
                match decoded.pdu {
                    // The server's way to the plugin host closed: this
                    // link has nothing more to carry.
                    Pdu::PluginFrame(PluginFrame { data }) if data.is_empty() => {
                        return Some("the plugin host there went away".into());
                    }
                    Pdu::PluginFrame(PluginFrame { data }) => heard(Heard::Frame(data)),
                    Pdu::ErrorResponse(codec::ErrorResponse { reason }) => return Some(reason),
                    // The server's probe of a client it has not heard
                    // from, answered as any client answers it: one that
                    // does not is taken for gone, and dropped.
                    Pdu::Ping(_) if decoded.serial == 0 => {
                        if let Err(err) = write_pdu(&mut *stream, &Pdu::Pong(Pong {}), 0).await {
                            return failed(err);
                        }
                    }
                    // The answers that say a frame left, and whatever
                    // else a server tells every client.
                    _ => {}
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::client::TlsWay;
    use codec::UnitResponse;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc::{channel, Receiver};

    const WAIT: Duration = Duration::from_secs(10);

    /// A link to a socket in `dir`, the server's end of it, and what the
    /// link's holder hears.
    fn linked(dir: &std::path::Path) -> (PluginLink, std::os::unix::net::UnixStream, Receiver<Heard>) {
        linked_asking(dir, KEEPALIVE_INTERVAL)
    }

    /// [`linked`], the server asked whether it is there after `quiet`.
    fn linked_asking(
        dir: &std::path::Path,
        quiet: Duration,
    ) -> (PluginLink, std::os::unix::net::UnixStream, Receiver<Heard>) {
        let path = dir.join("s");
        let listener = UnixListener::bind(&path).unwrap();
        let (tx, heard) = channel();
        let link = PluginLink::open_asking(PluginWay::Unix { path }, quiet, move |what| {
            let _ = tx.send(what);
        })
        .unwrap();
        let (mut server, _) = listener.accept().unwrap();
        // Before anything else, it asks for no pane's contents.
        let first = Pdu::decode(&mut server).unwrap();
        assert!(matches!(first.pdu, Pdu::SubscribeEvents(_)) && first.serial == 1);
        Pdu::UnitResponse(UnitResponse {}).encode(&mut server, 1).unwrap();
        (link, server, heard)
    }

    #[test]
    fn frames_go_to_the_server_in_order_and_what_it_pushes_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let (link, mut server, heard) = linked(dir.path());
        assert!(link.send(b"one".to_vec()));
        assert!(link.send(b"two".to_vec()));
        for (serial, expected) in [(2, b"one"), (3, b"two")] {
            let decoded = Pdu::decode(&mut server).unwrap();
            assert_eq!(decoded.serial, serial);
            let Pdu::PluginFrame(PluginFrame { data }) = decoded.pdu else {
                panic!("a plugin frame");
            };
            assert_eq!(data, expected);
            // Its answer says only that it left: nothing is heard of it.
            Pdu::UnitResponse(UnitResponse {}).encode(&mut server, serial).unwrap();
        }
        Pdu::PluginFrame(PluginFrame { data: b"back".to_vec() })
            .encode(&mut server, 0)
            .unwrap();
        match heard.recv_timeout(WAIT).unwrap() {
            Heard::Frame(data) => assert_eq!(data, b"back"),
            Heard::Ended(why) => panic!("ended: {}", why),
        }
        // The server's probe is answered, as any client answers it.
        Pdu::Ping(codec::Ping {}).encode(&mut server, 0).unwrap();
        let answer = Pdu::decode(&mut server).unwrap();
        assert!(matches!(answer.pdu, Pdu::Pong(_)) && answer.serial == 0);
        // An empty frame is the server's way to the plugin host closing.
        Pdu::PluginFrame(PluginFrame { data: Vec::new() })
            .encode(&mut server, 0)
            .unwrap();
        assert!(matches!(heard.recv_timeout(WAIT).unwrap(), Heard::Ended(_)));
        assert!(heard.recv_timeout(Duration::from_millis(200)).is_err(), "said once");
    }

    #[test]
    fn a_server_that_goes_silent_is_asked_and_then_given_up_on() {
        let dir = tempfile::tempdir().unwrap();
        let quiet = Duration::from_millis(150);
        let (link, mut server, heard) = linked_asking(dir.path(), quiet);
        // Nothing heard for a while: it is asked whether it is there, and
        // one that says so keeps the link.
        for _ in 0..2 {
            let asked = Pdu::decode(&mut server).unwrap();
            assert!(matches!(asked.pdu, Pdu::Ping(_)) && asked.serial != 0);
            Pdu::Pong(Pong {}).encode(&mut server, asked.serial).unwrap();
        }
        assert!(link.send(b"still".to_vec()));
        let sent = Pdu::decode(&mut server).unwrap();
        assert!(matches!(sent.pdu, Pdu::PluginFrame(_)));
        Pdu::UnitResponse(UnitResponse {}).encode(&mut server, sent.serial).unwrap();
        // Asked again and silent -- its connection still open, as one that
        // went without a word is -- the link ends, once.
        let asked = Pdu::decode(&mut server).unwrap();
        assert!(matches!(asked.pdu, Pdu::Ping(_)));
        match heard.recv_timeout(WAIT).unwrap() {
            Heard::Ended(why) => assert!(why.contains("stopped answering"), "{why}"),
            Heard::Frame(_) => panic!("a frame"),
        }
        assert!(heard.recv_timeout(quiet * 3).is_err(), "said once");
        drop(server);
    }

    #[test]
    fn a_link_let_go_closes_its_connection_and_says_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (link, mut server, heard) = linked(dir.path());
        assert!(link.send(b"one".to_vec()));
        assert!(Pdu::decode(&mut server).is_ok());
        drop(link);
        // The server sees the connection close.
        server.set_read_timeout(Some(WAIT)).unwrap();
        assert!(Pdu::decode(&mut server).is_err());
        assert!(heard.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn one_that_cannot_be_opened_or_loses_its_server_says_so_once() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, heard) = channel();
        let nowhere = PluginLink::open(PluginWay::Unix { path: dir.path().join("none") }, move |what| {
            let _ = tx.send(what);
        })
        .unwrap();
        assert!(matches!(heard.recv_timeout(WAIT).unwrap(), Heard::Ended(_)));
        // What is sent to it goes nowhere, and is not waited for.
        nowhere.send(b"lost".to_vec());

        let (link, server, heard) = linked(dir.path());
        drop(server);
        assert!(matches!(heard.recv_timeout(WAIT).unwrap(), Heard::Ended(_)));
        link.send(b"late".to_vec());
        assert!(heard.recv_timeout(Duration::from_millis(300)).is_err(), "said once");
    }

    #[test]
    fn a_command_carries_the_link_and_goes_with_it() {
        // `cat` hands back what it is sent: a frame for the host comes
        // back as one from it, and the empty one that closes the way.
        let (tx, heard) = channel();
        let link = PluginLink::open(PluginWay::Command { argv: vec!["cat".into()] }, move |what| {
            let _ = tx.send(what);
        })
        .unwrap();
        assert!(link.send(b"round".to_vec()));
        match heard.recv_timeout(WAIT).unwrap() {
            Heard::Frame(data) => assert_eq!(data, b"round"),
            Heard::Ended(why) => panic!("ended: {}", why),
        }
        assert!(link.send(Vec::new()));
        assert!(matches!(heard.recv_timeout(WAIT).unwrap(), Heard::Ended(_)));

        // One that cannot be run says so, once.
        let (tx, heard) = channel();
        let none = PluginLink::open(
            PluginWay::Command { argv: vec!["/nonexistent/thinkterm-proxy".into()] },
            move |what| {
                let _ = tx.send(what);
            },
        )
        .unwrap();
        assert!(matches!(heard.recv_timeout(WAIT).unwrap(), Heard::Ended(_)));
        drop(none);
        assert!(heard.recv_timeout(Duration::from_millis(200)).is_err(), "said once");
    }

    #[test]
    fn a_tls_connection_carries_the_link() {
        use openssl::asn1::Asn1Time;
        use openssl::bn::BigNum;
        use openssl::hash::MessageDigest;
        use openssl::pkey::PKey;
        use openssl::rsa::Rsa;
        use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};
        use openssl::x509::{X509Name, X509};

        // One certificate, signed by itself: the server's, the one the
        // client shows, and the one the client trusts.
        let dir = tempfile::tempdir().unwrap();
        let pem = dir.path().join("cert.pem");
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut name = X509Name::builder().unwrap();
        name.append_entry_by_text("CN", "localhost").unwrap();
        let name = name.build();
        let mut cert = X509::builder().unwrap();
        cert.set_version(2).unwrap();
        cert.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
            .unwrap();
        cert.set_subject_name(&name).unwrap();
        cert.set_issuer_name(&name).unwrap();
        cert.set_pubkey(&key).unwrap();
        cert.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
        cert.set_not_after(&Asn1Time::days_from_now(1).unwrap()).unwrap();
        cert.sign(&key, MessageDigest::sha256()).unwrap();
        let mut both = cert.build().to_pem().unwrap();
        both.extend(key.private_key_to_pem_pkcs8().unwrap());
        std::fs::write(&pem, both).unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
        acceptor.set_certificate_file(&pem, SslFiletype::PEM).unwrap();
        acceptor.set_private_key_file(&pem, SslFiletype::PEM).unwrap();
        let acceptor = acceptor.build();

        let (tx, heard) = channel();
        let way = PluginWay::Tls(TlsWay {
            client: config::TlsDomainClient {
                name: "server-a".into(),
                remote_address: address.to_string(),
                pem_root_certs: vec![pem.clone()],
                // The certificate names no address.
                accept_invalid_hostnames: true,
                read_timeout: WAIT,
                write_timeout: WAIT,
                ..Default::default()
            },
            creds_cert: Some(pem.clone()),
            creds_ca: None,
            network: wezterm_ssh::Network::System,
        });
        let link = PluginLink::open(way, move |what| {
            let _ = tx.send(what);
        })
        .unwrap();
        let (tcp, _) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(WAIT)).unwrap();
        let mut server = acceptor.accept(tcp).unwrap();
        let first = Pdu::decode(&mut server).unwrap();
        assert!(matches!(first.pdu, Pdu::SubscribeEvents(_)) && first.serial == 1);
        Pdu::UnitResponse(UnitResponse {}).encode(&mut server, 1).unwrap();

        assert!(link.send(b"one".to_vec()));
        let decoded = Pdu::decode(&mut server).unwrap();
        let Pdu::PluginFrame(PluginFrame { data }) = decoded.pdu else {
            panic!("a plugin frame");
        };
        assert_eq!((decoded.serial, data.as_slice()), (2, &b"one"[..]));
        // Two at once, as a host that draws a lot sends them: both come.
        for back in [&b"back"[..], &b"again"[..]] {
            Pdu::PluginFrame(PluginFrame { data: back.to_vec() })
                .encode(&mut server, 0)
                .unwrap();
        }
        for back in [&b"back"[..], &b"again"[..]] {
            match heard.recv_timeout(WAIT).unwrap() {
                Heard::Frame(data) => assert_eq!(data, back),
                Heard::Ended(why) => panic!("ended: {}", why),
            }
        }
        Pdu::Ping(codec::Ping {}).encode(&mut server, 0).unwrap();
        let answer = Pdu::decode(&mut server).unwrap();
        assert!(matches!(answer.pdu, Pdu::Pong(_)) && answer.serial == 0);
        // Let go, the connection closes and nothing is said of it.
        drop(link);
        assert!(Pdu::decode(&mut server).is_err());
        assert!(heard.recv_timeout(Duration::from_millis(300)).is_err());
    }
}
