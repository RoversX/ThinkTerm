//! The web listener: one TCP port per `web_servers` entry, plain or TLS,
//! whose connections go to `web_http::serve` on the connection executor.

use anyhow::{anyhow, bail, Context};
use async_ossl::AsyncSslStream;
use config::WebServer;
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};
use smol::Async;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use wezterm_mux_server_impl::web_auth::WEB_TOKENS;
use wezterm_mux_server_impl::web_http::{serve, serve_seated, PreAuthSeat, WebSite};

/// A TLS handshake is given this long in total; the accept thread never
/// waits on it at all.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often expired tokens are swept and their connections dropped.
const TOKEN_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

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
    wezterm_mux_server_impl::connections::spawn(async {
        loop {
            smol::Timer::after(TOKEN_SWEEP_INTERVAL).await;
            WEB_TOKENS.sweep();
        }
    });
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
    let (host, _) = server.host_and_port();
    let unspecified = host
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_unspecified());
    if unspecified && server.allowed_origins.is_empty() {
        bail!(
            "web server {} listens on every address but names no allowed_origins; \
             a browser reaching it by hostname would be refused. List the origins \
             (e.g. \"https://host.example:port\") or bind one address",
            server.bind_address
        );
    }
    if !server.is_loopback() && !server.uses_tls() && server.require_tls_off_loopback {
        bail!(
            "web server {} is not on loopback and has no TLS; browsers only expose WebGPU \
             to secure contexts, so it could not render. Add pem_cert/pem_private_key, \
             reach it through `ssh -L`, or set require_tls_off_loopback = false",
            server.bind_address
        );
    }
    let acceptor = if server.uses_tls() {
        Some(build_acceptor(server)?)
    } else {
        None
    };
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
    log::error!(
        "listening for web clients on {}{}",
        server.bind_address,
        if acceptor.is_some() { " with TLS" } else { "" }
    );

    std::thread::Builder::new()
        .name(format!("web-accept-{}", server.bind_address))
        .spawn(move || accept_loop(listener, acceptor, site))
        .context("spawning the web accept thread")?;
    Ok(())
}

/// How long the accept loop pauses after a failed accept. A refused
/// peer costs nothing; an exhausted descriptor table would otherwise spin.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

fn accept_loop(listener: TcpListener, acceptor: Option<SslAcceptor>, site: Arc<WebSite>) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
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
