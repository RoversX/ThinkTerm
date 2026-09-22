use anyhow::{anyhow, Context, Error};
use async_ossl::AsyncSslStream;
use config::TlsDomainServer;
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod, SslStream, SslVerifyMode};
use openssl::x509::X509;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use wezterm_mux_server_impl::PKI;

const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_TLS_HANDSHAKES: usize = 16;
/// At most one warning per five seconds per listener, even under a flood.
#[derive(Default)]
struct AdmissionWarnings {
    last: Option<Instant>,
    rejected: usize,
}
impl AdmissionWarnings {
    fn rejected(&mut self, now: Instant) -> Option<usize> {
        self.rejected = self.rejected.saturating_add(1);
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(5))
        {
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.rejected))
    }
}

struct HandshakePermit(Arc<AtomicUsize>);
impl HandshakePermit {
    fn acquire(count: &Arc<AtomicUsize>) -> Option<Self> {
        count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_TLS_HANDSHAKES).then_some(n + 1)
            })
            .ok()?;
        Some(Self(Arc::clone(count)))
    }
}
impl Drop for HandshakePermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn accept_with_deadline(
    acceptor: Arc<SslAcceptor>,
    stream: TcpStream,
    timeout: Duration,
    permit: HandshakePermit,
) -> anyhow::Result<SslStream<TcpStream>> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let guard = stream.try_clone()?;
    let handshake = smol::unblock(move || {
        // Keep the slot until the blocking operation actually exits, including
        // after its asynchronous waiter has timed out.
        let _permit = permit;
        acceptor.accept(stream)
    });
    let outcome = smol::future::or(async { Some(handshake.await) }, async {
        smol::Timer::after(timeout).await;
        None
    })
    .await;
    let tls = match outcome {
        Some(Ok(tls)) => tls,
        Some(Err(error)) => return Err(anyhow!("TLS handshake failed: {error}")),
        None => {
            let _ = guard.shutdown(std::net::Shutdown::Both);
            anyhow::bail!("TLS handshake deadline exceeded");
        }
    };
    tls.get_ref().set_read_timeout(None)?;
    tls.get_ref().set_write_timeout(None)?;
    Ok(tls)
}

struct OpenSSLNetListener {
    acceptor: Arc<SslAcceptor>,
    listener: TcpListener,
}

impl OpenSSLNetListener {
    pub fn new(listener: TcpListener, acceptor: SslAcceptor) -> Self {
        Self {
            listener,
            acceptor: Arc::new(acceptor),
        }
    }

    /// Authenticates the peer.
    /// The requirements are:
    /// * The peer must have a certificate
    /// * The peer certificate must be trusted
    /// * The peer certificate must include a CN string that is
    ///   either an exact match for the unix username of the
    ///   user running this mux server instance, or must match
    ///   a special encoded prefix set up by a proprietary PKI
    ///   infrastructure in an environment used by the author.
    fn verify_peer_cert<T>(stream: &SslStream<T>) -> anyhow::Result<()> {
        let cert = stream
            .ssl()
            .peer_certificate()
            .ok_or_else(|| anyhow!("no peer cert"))?;
        let subject = cert.subject_name();
        let cn = subject
            .entries_by_nid(openssl::nid::Nid::COMMONNAME)
            .next()
            .ok_or_else(|| anyhow!("cert has no CN"))?;
        let cn_str = cn.data().as_utf8()?.to_string();

        let wanted_unix_name = std::env::var("USER")?;

        if wanted_unix_name == cn_str {
            log::trace!(
                "Peer certificate CN `{}` == $USER `{}`",
                cn_str,
                wanted_unix_name
            );
            Ok(())
        } else {
            // Some environments that are used by the author of this
            // program encode the CN in the form `user:unixname/DATA`
            let maybe_encoded = format!("user:{}/", wanted_unix_name);
            if cn_str.starts_with(&maybe_encoded) {
                log::trace!(
                    "Peer certificate CN `{}` matches $USER `{}`",
                    cn_str,
                    wanted_unix_name
                );
                Ok(())
            } else {
                anyhow::bail!("CN `{}` did not match $USER `{}`", cn_str, wanted_unix_name);
            }
        }
    }

    fn run(&mut self) {
        let pending = Arc::new(AtomicUsize::new(0));
        let mut warnings = AdmissionWarnings::default();
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    let Some(permit) = HandshakePermit::acquire(&pending) else {
                        if let Some(rejected) = warnings.rejected(Instant::now()) {
                            log::warn!("TLS handshake capacity ({MAX_TLS_HANDSHAKES}) reached: rejected {rejected} new connections since the last warning; pending handshakes have a {}s deadline", TLS_HANDSHAKE_TIMEOUT.as_secs());
                        }
                        continue;
                    };
                    stream.set_nodelay(true).ok();
                    let acceptor = self.acceptor.clone();
                    wezterm_mux_server_impl::connections::spawn(async move {
                        let stream = match accept_with_deadline(
                            acceptor,
                            stream,
                            TLS_HANDSHAKE_TIMEOUT,
                            permit,
                        )
                        .await
                        {
                            Ok(stream) => stream,
                            Err(err) => {
                                log::warn!("{err}");
                                return;
                            }
                        };
                        if let Err(err) = Self::verify_peer_cert(&stream) {
                            log::error!("problem with peer cert: {err}");
                            return;
                        }
                        if let Err(err) = wezterm_mux_server_impl::dispatch::process(
                            AsyncSslStream::new(stream),
                            wezterm_mux_server_impl::sessionhandler::ConnectionPeer::Tls,
                        )
                        .await
                        {
                            log::error!("process: {err:?}");
                        }
                    });
                }
                Err(err) => {
                    log::error!("accept failed: {err}");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

pub fn spawn_tls_listener(tls_server: &TlsDomainServer) -> Result<(), Error> {
    openssl::init();

    let mut acceptor = SslAcceptor::mozilla_modern(SslMethod::tls())?;

    let cert_file = tls_server
        .pem_cert
        .clone()
        .unwrap_or_else(|| PKI.server_pem());
    acceptor
        .set_certificate_file(&cert_file, SslFiletype::PEM)
        .context(format!(
            "set_certificate_file to {} for TLS listener",
            cert_file.display()
        ))?;

    if let Some(chain_file) = tls_server.pem_ca.as_ref() {
        acceptor
            .set_certificate_chain_file(&chain_file)
            .context(format!(
                "set_certificate_chain_file to {} for TLS listener",
                chain_file.display()
            ))?;
    }

    let key_file = tls_server
        .pem_private_key
        .clone()
        .unwrap_or_else(|| PKI.server_pem());
    acceptor
        .set_private_key_file(&key_file, SslFiletype::PEM)
        .context(format!(
            "set_private_key_file to {} for TLS listener",
            key_file.display()
        ))?;

    fn load_cert(name: &Path) -> anyhow::Result<X509> {
        let cert_bytes = std::fs::read(name)?;
        log::trace!("loaded {}", name.display());
        Ok(X509::from_pem(&cert_bytes)?)
    }
    for name in &tls_server.pem_root_certs {
        if name.is_dir() {
            for entry in std::fs::read_dir(name)? {
                if let Ok(cert) = load_cert(&entry?.path()) {
                    acceptor.cert_store_mut().add_cert(cert).ok();
                }
            }
        } else {
            acceptor.cert_store_mut().add_cert(load_cert(name)?)?;
        }
    }

    acceptor
        .cert_store_mut()
        .add_cert(load_cert(&PKI.ca_pem())?)?;

    acceptor.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);

    let acceptor = acceptor.build();

    log::error!("listening with TLS on {:?}", tls_server.bind_address);

    let mut net_listener = OpenSSLNetListener::new(
        TcpListener::bind(&tls_server.bind_address).with_context(|| {
            format!(
                "error binding to mux_server_bind_address {}",
                tls_server.bind_address,
            )
        })?,
        acceptor,
    );
    std::thread::spawn(move || {
        net_listener.run();
    });
    Ok(())
}

#[cfg(test)]
pub(crate) mod deadline_tests {
    use super::*;
    pub(crate) fn test_acceptor() -> SslAcceptor {
        use openssl::{
            asn1::Asn1Time, hash::MessageDigest, pkey::PKey, rsa::Rsa, x509::X509NameBuilder,
        };
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "localhost").unwrap();
        let name = name.build();
        let mut cert = X509::builder().unwrap();
        cert.set_version(2).unwrap();
        cert.set_subject_name(&name).unwrap();
        cert.set_issuer_name(&name).unwrap();
        cert.set_pubkey(&key).unwrap();
        cert.set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        cert.set_not_after(&Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        cert.sign(&key, MessageDigest::sha256()).unwrap();
        let mut acceptor = SslAcceptor::mozilla_modern(SslMethod::tls()).unwrap();
        acceptor.set_private_key(&key).unwrap();
        acceptor.set_certificate(&cert.build()).unwrap();
        acceptor.build()
    }

    #[test]
    fn valid_tls_connection_completes_and_clears_handshake_timeouts() {
        use std::io::{Read, Write};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut connector = openssl::ssl::SslConnector::builder(SslMethod::tls()).unwrap();
            // This fixture tests transport scheduling; production peer validation remains mandatory.
            connector.set_verify(SslVerifyMode::NONE);
            let mut stream = connector
                .build()
                .connect("localhost", TcpStream::connect(address).unwrap())
                .unwrap();
            stream.write_all(b"ping").unwrap();
        });
        let (stream, _) = listener.accept().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let mut tls = smol::block_on(accept_with_deadline(
            Arc::new(test_acceptor()),
            stream,
            Duration::from_secs(3),
            HandshakePermit::acquire(&count).unwrap(),
        ))
        .unwrap();
        assert_eq!(tls.get_ref().read_timeout().unwrap(), None);
        assert_eq!(tls.get_ref().write_timeout().unwrap(), None);
        let mut bytes = [0; 4];
        tls.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ping");
        assert_eq!(count.load(Ordering::Acquire), 0);
        assert!(
            OpenSSLNetListener::verify_peer_cert(&tls).is_err(),
            "transport success must not bypass client certificate authentication"
        );
        client.join().unwrap();
    }

    #[test]
    fn admission_warnings_are_rate_limited_and_count_rejections() {
        let start = Instant::now();
        let mut warnings = AdmissionWarnings::default();
        assert_eq!(warnings.rejected(start), Some(1));
        for _ in 0..100 {
            assert_eq!(warnings.rejected(start + Duration::from_secs(1)), None);
        }
        assert_eq!(warnings.rejected(start + Duration::from_secs(5)), Some(101));
    }

    #[test]
    fn stalled_handshakes_expire_and_admission_is_bounded() {
        let count = Arc::new(AtomicUsize::new(0));
        let permits: Vec<_> = (0..MAX_TLS_HANDSHAKES - 1)
            .map(|_| HandshakePermit::acquire(&count).unwrap())
            .collect();
        let last_permit = HandshakePermit::acquire(&count).unwrap();
        assert!(HandshakePermit::acquire(&count).is_none());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let _silent = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let acceptor = Arc::new(test_acceptor());
        let start = std::time::Instant::now();
        assert!(smol::block_on(accept_with_deadline(
            acceptor,
            stream,
            Duration::from_millis(100),
            last_permit
        ))
        .is_err());
        assert!(start.elapsed() >= Duration::from_millis(80));
        assert!(start.elapsed() < Duration::from_secs(2));
        while count.load(Ordering::Acquire) != MAX_TLS_HANDSHAKES - 1
            && start.elapsed() < Duration::from_secs(2)
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(count.load(Ordering::Acquire), MAX_TLS_HANDSHAKES - 1);
        let replacement =
            HandshakePermit::acquire(&count).expect("expired connection frees admission");
        assert!(HandshakePermit::acquire(&count).is_none());
        drop(replacement);
        drop(permits);
        assert_eq!(count.load(Ordering::Acquire), 0);
    }
}
