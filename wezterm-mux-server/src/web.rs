//! The web listener: one TCP port per `web_servers` entry, plain or TLS,
//! whose connections go to `web_http::serve` on the connection executor.

use anyhow::{anyhow, bail, Context};
use async_ossl::AsyncSslStream;
use config::WebServer;
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};
use smol::Async;
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wezterm_mux_server_impl::web_auth::WEB_TOKENS;
use wezterm_mux_server_impl::web_http::{serve, serve_seated, PreAuthSeat, WebSite};

/// A TLS handshake is given this long in total; the accept thread never
/// waits on it at all.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often expired tokens are swept and their connections dropped.
const TOKEN_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// The listeners that are up, and the flag that ends each one.
///
/// Stopping is two moves, because the accept loop blocks inside
/// `accept()` and owns its listener: raise the flag, then open one
/// connection to the port so the loop wakes and sees it. Dropping the
/// listener from another thread is not available to us.
static LISTENERS: Mutex<BTreeMap<String, Listening>> = Mutex::new(BTreeMap::new());

struct Listening {
    /// What the listener runs with: the entry as given, plus the
    /// certificate it made for itself when the entry named none.
    effective: WebServer,
    fingerprint: Option<String>,
    stop: Arc<AtomicBool>,
    /// The address `accept()` actually returned, not the configured
    /// string: an unspecified bind has to be woken through a real address.
    local: SocketAddr,
    /// Set by the loop as it returns: the port is free from then on.
    done: Option<Arc<AtomicBool>>,
}

/// The bind addresses currently accepting, in configuration order.
pub fn listening() -> Vec<String> {
    LISTENERS.lock().map_or_else(|e| e.into_inner(), |g| g).keys().cloned().collect()
}

/// The entry a live listener runs with.
pub fn effective(bind_address: &str) -> Option<WebServer> {
    LISTENERS
        .lock()
        .map_or_else(|e| e.into_inner(), |g| g)
        .get(bind_address)
        .map(|l| l.effective.clone())
}

/// Captured from the loaded SSL context; disk rotation cannot change a live
/// listener's identity until that listener is restarted.
pub fn certificates() -> Vec<(String, String)> {
    LISTENERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(|(bind, live)| {
            live.fingerprint
                .as_ref()
                .map(|fp| (bind.clone(), fp.clone()))
        })
        .collect()
}
fn certificate_fingerprint(acceptor: &SslAcceptor) -> anyhow::Result<String> {
    let certificate = acceptor
        .context()
        .certificate()
        .context("TLS listener has no certificate")?;
    let digest = certificate.digest(openssl::hash::MessageDigest::sha256())?;
    Ok(digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":"))
}

pub fn is_listening(bind_address: &str) -> bool {
    LISTENERS
        .lock()
        .map_or_else(|e| e.into_inner(), |g| g)
        .contains_key(bind_address)
}

/// The address to knock on to wake a blocking `accept`. A listener bound to
/// every address is not reachable *at* that address, so knock on loopback.
fn wake_address(local: SocketAddr) -> SocketAddr {
    if !local.ip().is_unspecified() {
        return local;
    }
    match local {
        SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::LOCALHOST, local.port())),
        SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::LOCALHOST, local.port())),
    }
}

/// Stop the listener on this address. `false` if nothing was listening.
///
/// The port is free once this returns: the loop is woken, sees the flag and
/// drops the listener before the knock is answered.
pub fn stop_web_listener(bind_address: &str) -> bool {
    let Some(entry) = LISTENERS
        .lock()
        .map_or_else(|e| e.into_inner(), |g| g)
        .remove(bind_address)
    else {
        return false;
    };
    entry.stop.store(true, Ordering::SeqCst);
    // The loop waits in `poll` with a timeout and looks at the flag
    // between waits (a `shutdown` on a listening socket is ENOTCONN on
    // macOS and does not wake `accept`); the knock only shortens the wait
    // where it lands on this listener rather than a more specific one.
    let _ = TcpStream::connect_timeout(&wake_address(entry.local), Duration::from_secs(1));
    if let Some(done) = entry.done.as_ref() {
        // Bounded: the port is free once the loop has returned.
        let waited = std::time::Instant::now();
        while !done.load(Ordering::SeqCst) && waited.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    log::error!("stopped listening for web clients on {bind_address}");
    true
}

/// Point the token store at its file. One store serves every web listener,
/// so the first entry's choice wins; the others are told.
pub fn configure_tokens(servers: &[WebServer]) -> anyhow::Result<()> {
    let Some(first) = servers.first() else {
        return Ok(());
    };
    WEB_TOKENS
        .configure(first.token_file.clone())
        .context("loading the web token store")?;
    for other in &servers[1..] {
        if other.token_file != first.token_file {
            log::warn!(
                "web_servers share one token store; token_file on {} is ignored in favour of {:?}",
                other.bind_address,
                first.token_file
            );
        }
    }
    // A token's lifetime ends when it says, whether or not anyone touches
    // the store again.
    //
    // Once per process. This used to run at startup only; the settings
    // switch calls it again on every "on", and each of those left another
    //ever-running sweep behind -- more timers doing the same work, and one
    // more every time the switch is flipped.
    static SWEEPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !SWEEPING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        wezterm_mux_server_impl::connections::spawn(async {
            loop {
                smol::Timer::after(TOKEN_SWEEP_INTERVAL).await;
                WEB_TOKENS.sweep();
            }
        });
    }
    Ok(())
}

/// Where the browser bundle lives: the configured directory, an override
/// for development, or the share directory next to the executable.
fn resolve_static_dir(server: &WebServer) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![];
    if let Some(dir) = &server.static_dir {
        candidates.push(dir.clone());
    }
    if let Some(dir) = std::env::var_os("THINKTERM_WEB_STATIC_DIR") {
        candidates.push(dir.into());
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin) = exe.parent() {
            candidates.push(bin.join("../share/thinkterm/web"));
            candidates.push(bin.join("../Resources/web"));
            // The Windows zip and installer are flat: everything beside the
            // executables, the bundle in web/.
            candidates.push(bin.join("web"));
        }
    }
    for candidate in candidates {
        if candidate.join("index.html").is_file() {
            return Some(candidate);
        }
    }
    None
}

fn build_acceptor(server: &WebServer) -> anyhow::Result<SslAcceptor> {
    openssl::init();
    let (Some(cert), Some(key)) = (&server.pem_cert, &server.pem_private_key) else {
        bail!(
            "web server {}: pem_cert and pem_private_key must both be set for TLS",
            server.bind_address
        );
    };
    let mut acceptor = SslAcceptor::mozilla_modern(SslMethod::tls())?;
    acceptor
        .set_certificate_file(cert, SslFiletype::PEM)
        .with_context(|| format!("set_certificate_file {}", cert.display()))?;
    acceptor
        .set_private_key_file(key, SslFiletype::PEM)
        .with_context(|| format!("set_private_key_file {}", key.display()))?;
    if let Some(chain) = &server.pem_ca {
        acceptor
            .set_certificate_chain_file(chain)
            .with_context(|| format!("set_certificate_chain_file {}", chain.display()))?;
    }
    // No client certificate: a browser has none. The token is the
    // credential; TLS is here for the secure context and the wire.
    Ok(acceptor.build())
}

pub fn spawn_web_listener(server: &WebServer) -> anyhow::Result<()> {
    if server.tls_half_configured() {
        bail!(
            "web server {}: pem_cert and pem_private_key must both be set for TLS",
            server.bind_address
        );
    }
    // Off loopback a browser needs https for WebGPU. Without a
    // certificate of the user's, the listener makes its own: self-signed,
    // for the machine's names and addresses, kept across restarts. The
    // browser warns once; a name it cannot verify is still a secure
    // context once accepted.
    let mut effective = server.clone();
    if !server.is_loopback() && !server.uses_tls() && server.require_tls_off_loopback {
        let tls = wezterm_mux_server_impl::web_tls::ensure(&config::local_addresses())
            .with_context(|| {
                format!("web server {}: making its certificate", server.bind_address)
            })?;
        log::error!(
            "web server {}: {} self-signed certificate {} (SHA-256 {}); compare this fingerprint with the browser certificate before trusting",
            server.bind_address,
            if tls.generated { "made a" } else { "using the" },
            tls.cert.display(),
            tls.fingerprint
        );
        effective.pem_cert = Some(tls.cert);
        effective.pem_private_key = Some(tls.key);
    }
    let server = &effective;
    let acceptor = if server.uses_tls() {
        Some(build_acceptor(server)?)
    } else {
        None
    };
    let fingerprint = acceptor.as_ref().map(certificate_fingerprint).transpose()?;
    let static_dir = resolve_static_dir(server);
    match &static_dir {
        Some(dir) => log::info!("web bundle for {} at {}", server.bind_address, dir.display()),
        None => log::warn!(
            "no web bundle found for {}; the port accepts WebSockets but serves no page \
             (set static_dir or install the bundle under share/thinkterm/web)",
            server.bind_address
        ),
    }
    let username = config::username_from_env()
        .map_err(|e| anyhow!("resolving the server's user for web sessions: {e}"))?;
    let site = Arc::new(WebSite::from_config(server, static_dir, username));

    let listener = TcpListener::bind(&server.bind_address)
        .with_context(|| format!("binding web server to {}", server.bind_address))?;
    let local = listener
        .local_addr()
        .with_context(|| format!("reading the address of the web server on {}", server.bind_address))?;
    log::error!(
        "listening for web clients on {}{}",
        server.bind_address,
        if acceptor.is_some() { " with TLS" } else { "" }
    );

    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    // Registered before the thread starts, so a stop that arrives in the
    // same breath as the start still finds it.
    LISTENERS
        .lock()
        .map_or_else(|e| e.into_inner(), |g| g)
        .insert(
            server.bind_address.clone(),
            Listening {
                effective: effective.clone(),
                fingerprint,
                stop: Arc::clone(&stop),
                local,
                done: Some(Arc::clone(&done)),
            },
        );
    let bind_address = server.bind_address.clone();
    match std::thread::Builder::new()
        .name(format!("web-accept-{}", server.bind_address))
        .spawn(move || accept_loop(listener, acceptor, site, stop, done))
    {
        Ok(_) => {
            // The other half of `disconnect_all`, which stops admissions
            // when a listener goes down. Done here, on every path that
            // opens a port -- startup, the takeover thread, the settings
            // switch -- because an "off" that lands before the takeover's
            // listener is up would otherwise leave a port open on which
            // every token is refused.
            wezterm_mux_server_impl::web_auth::WEB_TOKENS.resume_admitting();
            Ok(())
        }
        Err(err) => {
            // Nothing is accepting, so nothing may claim to be.
            LISTENERS
                .lock()
                .map_or_else(|e| e.into_inner(), |g| g)
                .remove(&bind_address);
            Err(err).context("spawning the web accept thread")
        }
    }
}

/// How long the accept loop pauses after a failed accept. A refused
/// peer costs nothing; an exhausted descriptor table would otherwise spin.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// How long the loop waits for a connection before it looks at the stop
/// flag again: a stop is seen within this, knock or no knock.
const ACCEPT_WAIT: Duration = Duration::from_millis(250);

/// Wait for a connection to be ready, `ACCEPT_WAIT` at most. Blocking
/// `accept` cannot be woken from another thread on every platform (a
/// `shutdown` of a listening socket is ENOTCONN on macOS), so the wait
/// is a `poll` and the accept never blocks.
fn connection_ready(listener: &TcpListener) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let mut fds = [libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, ACCEPT_WAIT.as_millis() as libc::c_int) };
        n > 0
    }
    #[cfg(not(unix))]
    {
        std::thread::sleep(Duration::from_millis(20));
        true
    }
}

fn accept_loop(
    listener: TcpListener,
    acceptor: Option<SslAcceptor>,
    site: Arc<WebSite>,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
) {
    let _ = listener.set_nonblocking(true);
    loop {
        // Checked before the connection is looked at, so the knock that
        // woke us is dropped rather than served.
        if stop.load(Ordering::SeqCst) {
            log::info!("web accept loop for {} is done", site.describe);
            done.store(true, Ordering::SeqCst);
            return;
        }
        if !connection_ready(&listener) {
            continue;
        }
        let stream = match listener.accept() {
            // Blocking again for the handshake; `Async::new` sets its own.
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                stream
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(err) => {
                if stop.load(Ordering::SeqCst) {
                    log::info!("web accept loop for {} is done", site.describe);
                    done.store(true, Ordering::SeqCst);
                    return;
                }
                // One bad client (or a moment without descriptors) is one
                // refused connection, not the end of the listener.
                log::error!("web accept failed on {}: {err}", site.describe);
                std::thread::sleep(ACCEPT_RETRY);
                continue;
            }
        };
        stream.set_nodelay(true).ok();
        let site = Arc::clone(&site);
        match &acceptor {
            None => match Async::new(stream) {
                Ok(stream) => {
                    wezterm_mux_server_impl::connections::spawn(serve(stream, site));
                }
                Err(err) => log::warn!("web connection not registered: {err}"),
            },
            Some(acceptor) => {
                // The seat is taken before the handshake: the handshake is
                // blocking OpenSSL on the blocking pool, and the seats are
                // what bound how many of its threads a crowd of strangers
                // can hold. No seat, no handshake -- the socket is simply
                // closed; a 503 cannot be written before TLS is up.
                let Some(seat) = PreAuthSeat::take() else {
                    log::warn!("web TLS connection refused: too many unauthenticated peers");
                    continue;
                };
                // The deadline has to reach the socket itself: dropping the
                // pool task cannot interrupt SSL_accept, and the per-call
                // timeouts restart with every dribbled byte.
                let guard = match stream.try_clone() {
                    Ok(guard) => guard,
                    Err(err) => {
                        log::warn!("web TLS connection not accepted: {err}");
                        continue;
                    }
                };
                let acceptor = acceptor.clone();
                wezterm_mux_server_impl::connections::spawn(async move {
                    stream.set_read_timeout(Some(TLS_HANDSHAKE_TIMEOUT)).ok();
                    stream.set_write_timeout(Some(TLS_HANDSHAKE_TIMEOUT)).ok();
                    let handshake = smol::unblock(move || acceptor.accept(stream));
                    let tls = smol::future::or(
                        async { Some(handshake.await) },
                        async {
                            smol::Timer::after(TLS_HANDSHAKE_TIMEOUT).await;
                            None
                        },
                    )
                    .await;
                    let tls = match tls {
                        Some(Ok(tls)) => tls,
                        Some(Err(err)) => {
                            log::warn!("web TLS handshake failed: {err}");
                            return;
                        }
                        None => {
                            log::warn!(
                                "web TLS handshake took longer than {TLS_HANDSHAKE_TIMEOUT:?}; \
                                 shutting the socket down"
                            );
                            let _ = guard.shutdown(std::net::Shutdown::Both);
                            return;
                        }
                    };
                    drop(guard);
                    tls.get_ref().set_read_timeout(None).ok();
                    tls.get_ref().set_write_timeout(None).ok();
                    match Async::new(AsyncSslStream::new(tls)) {
                        Ok(stream) => serve_seated(stream, site, seat).await,
                        Err(err) => log::warn!("web TLS connection not registered: {err}"),
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod certificate_tests {
    use super::*;
    #[test]
    fn identities_follow_live_listeners_and_ignore_plain_http() {
        let first = crate::ossl::deadline_tests::test_acceptor();
        let second = crate::ossl::deadline_tests::test_acceptor();
        let first_fp = certificate_fingerprint(&first).unwrap();
        let second_fp = certificate_fingerprint(&second).unwrap();
        assert_ne!(first_fp, second_fp);
        assert_eq!(first_fp.split(':').count(), 32);
        let binds = ["127.0.0.1:0", "127.0.0.2:0", "127.0.0.3:0"];
        for (bind, fingerprint) in
            binds
                .iter()
                .zip([Some(first_fp.clone()), Some(second_fp.clone()), None])
        {
            LISTENERS.lock().unwrap().insert(
                (*bind).into(),
                Listening {
                    effective: WebServer {
                        bind_address: (*bind).into(),
                        ..Default::default()
                    },
                    fingerprint,
                    stop: Arc::new(AtomicBool::new(false)),
                    local: bind.parse().unwrap(),
                    done: None,
                },
            );
        }
        let fingerprints = certificates();
        assert!(fingerprints.contains(&(binds[0].into(), first_fp.clone())));
        assert!(fingerprints.contains(&(binds[1].into(), second_fp)));
        assert!(!fingerprints.iter().any(|(bind, _)| bind == binds[2]));
        // Building a replacement context did not change the old listener.
        assert_eq!(certificate_fingerprint(&first).unwrap(), first_fp);
        for bind in binds {
            assert!(stop_web_listener(bind));
        }
        assert!(!certificates()
            .iter()
            .any(|(bind, _)| binds.contains(&bind.as_str())));
    }
}
