//! The HTTP side of a web listener: serve the browser client's bundle, and
//! turn a WebSocket upgrade into a mux connection once the token checks
//! out. The check happens here, before `dispatch` ever sees the peer: the
//! session handler answers `ListPanes` and `GetLines` to anyone, so an
//! unauthenticated socket must never reach it.
//!
//! Only the request head is parsed by hand (httparse); the WebSocket
//! framing after the upgrade is soketto's, through `web_stream`.

use crate::dispatch::process_stream;
use crate::sessionhandler::{ConnectionPeer, WebPeer};
use crate::web_auth::WEB_TOKENS;
use crate::web_stream::{Prefixed, WebStream};
use base64::Engine;
use config::{default_port, is_loopback_host, split_authority, Origin};
use futures::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use sha1::{Digest, Sha1};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The one subprotocol the browser client speaks. A token rides beside it
/// in the same header, the only request header a browser lets a page set.
pub const SUBPROTOCOL: &str = "thinkterm.v1";
/// The page asking this server to reach another machine for it
/// (`web_relay`): the same token, a different conversation.
pub const RELAY_SUBPROTOCOL: &str = "thinkterm.relay.v1";
pub const TOKEN_PROTOCOL_PREFIX: &str = "tt-token.";

const MAX_HEAD: usize = 16 * 1024;
/// The whole request head must arrive within this, not each byte of it.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Each piece of a response must be taken within this. A peer that opens
/// its window slower than that is holding a seat for nothing.
const WRITE_STALL: Duration = Duration::from_secs(15);
/// Files leave in pieces of this size; a response never holds more.
const WRITE_CHUNK: usize = 64 * 1024;
/// Connections allowed to sit before the token check at once. Beyond it a
/// new socket gets 503 and is closed: the port must not be able to exhaust
/// the process's descriptors, which the unix listener shares. The seat is
/// held for the whole of a plain response too, so slow readers of the
/// bundle are bounded by the same number.
const MAX_PRE_AUTH: usize = 128;

static PRE_AUTH: AtomicUsize = AtomicUsize::new(0);

/// One connection's seat before authentication; released on drop. The
/// TLS listener takes it before the handshake, which is the point: the
/// handshake is what a stranger can make cost the most.
pub struct PreAuthSeat;

impl PreAuthSeat {
    pub fn take() -> Option<Self> {
        if PRE_AUTH.fetch_add(1, Ordering::SeqCst) < MAX_PRE_AUTH {
            Some(Self)
        } else {
            PRE_AUTH.fetch_sub(1, Ordering::SeqCst);
            None
        }
    }
}

impl Drop for PreAuthSeat {
    fn drop(&mut self) {
        PRE_AUTH.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A request line for the log: never the query string, which is where a
/// bootstrap token would be.
fn describe(req: &Request) -> String {
    let path = req.path.split(['?', '#']).next().unwrap_or("");
    format!("{} {}", req.method, path)
}
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// What a listener needs to answer requests, derived once from its config.
/// Hosts and origins are compared taken apart -- scheme, host, the port
/// the URL means -- so `mux.example.net` and `mux.example.net:443` are
/// the same name, as they are to a browser.
#[derive(Debug)]
pub struct WebSite {
    /// Names, with the port they mean, that this listener answers to.
    hosts: Vec<(String, Option<u16>)>,
    /// Origins allowed to open the WebSocket.
    allowed_origins: Vec<Origin>,
    /// Whether the origins were configured, as opposed to derived from
    /// the bind address.
    configured_origins: bool,
    scheme: &'static str,
    /// The port clients reach us on, when the bind address has one.
    port: Option<u16>,
    /// A loopback listener is reached through `ssh -L` on whatever local
    /// port the user chose, so any loopback name is ours.
    loopback: bool,
    static_dir: Option<PathBuf>,
    /// The server's own user; every web session runs as that user.
    username: String,
    pub describe: String,
}

impl WebSite {
    pub fn from_config(
        server: &config::WebServer,
        static_dir: Option<PathBuf>,
        username: String,
    ) -> Self {
        let scheme = server.scheme();
        let allowed_origins: Vec<Origin> = server
            .effective_allowed_origins()
            .iter()
            .filter_map(|o| {
                let parsed = Origin::parse(o);
                if parsed.is_none() {
                    log::warn!("allowed_origins entry {o:?} is not an origin; ignored");
                }
                parsed
            })
            .collect();
        let mut hosts: Vec<(String, Option<u16>)> = server
            .url_hosts()
            .iter()
            .filter_map(|h| split_authority(h))
            .map(|(host, port)| (host, port.or(default_port(scheme))))
            .collect();
        // A page served from a configured origin reaches us by whatever
        // name it was told; those names are ours too, spelled as the
        // origin spells them (a `Host` has no scheme to default from).
        for origin in &allowed_origins {
            let name = (origin.host.clone(), origin.port_given);
            if !hosts.contains(&name) {
                hosts.push(name);
            }
        }
        let (_, port) = server.host_and_port();
        Self {
            hosts,
            allowed_origins,
            configured_origins: !server.allowed_origins.is_empty(),
            scheme,
            port: port.or(default_port(scheme)),
            loopback: server.is_loopback(),
            static_dir,
            username,
            describe: server.bind_address.clone(),
        }
    }

    /// The `Host` header taken apart, with the port it means.
    fn host_name(&self, host: &str) -> Option<(String, Option<u16>)> {
        let (name, port) = split_authority(host)?;
        Some((name, port.or(default_port(self.scheme))))
    }

    fn host_allowed(&self, host: &str) -> bool {
        let Some(name) = self.host_name(host) else {
            return false;
        };
        let Some(as_written) = split_authority(host) else {
            return false;
        };
        if self.hosts.contains(&name) || self.hosts.contains(&as_written) {
            return true;
        }
        let (host, port) = &name;
        // Loopback names cannot be rebound to point elsewhere, and a
        // loopback listener has no say in which local port a forward uses.
        if self.loopback && is_loopback_host(host) {
            return true;
        }
        // An IP literal cannot be a DNS-rebinding name; accept it when it
        // carries our port.
        host.parse::<std::net::IpAddr>().is_ok() && *port == self.port
    }

    /// Whether `origin` may open the socket, given the `Host` the same
    /// request carried.
    ///
    /// Configured origins are the whole answer when there are any.
    /// Otherwise the rule is same-origin: the page this listener served
    /// itself, by whatever name it was reached under. That covers every way
    /// the listener is legitimately reached -- an `ssh -L` forward on a
    /// local port of the user's choosing, an address the machine gained
    /// after the listener came up, a tailnet, a NAT -- because in all of
    /// them a browser sends the same authority in both headers.
    ///
    /// It used to also accept any non-loopback IP-literal origin carrying
    /// our port, whatever `Host` said, on the grounds that an IP literal
    /// cannot be a DNS-rebinding name. True, but it does not follow that
    /// every IP literal is ours: a page on an unrelated address could open
    /// this socket. It still needed a token, so this was depth rather than
    /// a way in, but the depth is worth having.
    fn origin_allowed(&self, origin: &str, host: Option<&str>) -> bool {
        let Some(origin) = Origin::parse(origin) else {
            return false;
        };
        if self.allowed_origins.iter().any(|o| o.same_as(&origin)) {
            return true;
        }
        if self.configured_origins {
            return false;
        }
        // `Host` has already been checked by the time this runs, so the two
        // agreeing is what a browser only produces same-origin: a
        // cross-origin request carries our host and the *other* page's
        // origin.
        origin.scheme == self.scheme
            && host.and_then(|h| self.host_name(h)) == Some((origin.host, origin.port))
    }
}

/// A parsed request head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Names lowercase.
    pub headers: Vec<(String, String)>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// A comma-separated header, each item trimmed; every occurrence of
    /// the header contributes.
    pub fn header_list(&self, name: &str) -> Vec<String> {
        self.headers
            .iter()
            .filter(|(n, _)| n == name)
            .flat_map(|(_, v)| v.split(',').map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect()
    }

    fn header_has_token(&self, name: &str, token: &str) -> bool {
        self.header_list(name)
            .iter()
            .any(|v| v.eq_ignore_ascii_case(token))
    }
}

/// Parse a request head. `Ok(None)` means more bytes are needed;
/// `Ok(Some((request, head_len)))` says how many bytes the head took.
pub fn parse_head(bytes: &[u8]) -> Result<Option<(Request, usize)>, String> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(bytes) {
        Ok(httparse::Status::Partial) => Ok(None),
        Ok(httparse::Status::Complete(len)) => {
            let method = req.method.unwrap_or("").to_string();
            let path = req.path.unwrap_or("/").to_string();
            let headers = req
                .headers
                .iter()
                .map(|h| {
                    (
                        h.name.to_ascii_lowercase(),
                        String::from_utf8_lossy(h.value).trim().to_string(),
                    )
                })
                .collect();
            Ok(Some((
                Request {
                    method,
                    path,
                    headers,
                },
                len,
            )))
        }
        Err(err) => Err(err.to_string()),
    }
}

/// Where a request goes.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// Serve this file from the bundle.
    Static(PathBuf),
    /// A WebSocket upgrade carrying a token; verified by the caller.
    /// `relay` is the page asking for another machine rather than this one.
    Upgrade { key: String, token: String, relay: bool },
    Reject { status: u16, reason: &'static str },
}

fn reject(status: u16, reason: &'static str) -> Route {
    Route::Reject { status, reason }
}

pub fn route(site: &WebSite, req: &Request) -> Route {
    if req.headers.iter().filter(|(n, _)| n == "host").count() > 1 {
        return reject(400, "One Host header, please");
    }
    match req.header("host") {
        Some(host) if site.host_allowed(host) => {}
        Some(_) => return reject(403, "Host not served here"),
        None => return reject(400, "Host header required"),
    }
    if req.method != "GET" {
        return reject(405, "Method not allowed");
    }

    let wants_upgrade = req.header_has_token("connection", "upgrade")
        && req
            .header("upgrade")
            .is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
    if !wants_upgrade {
        let Some(root) = &site.static_dir else {
            return reject(404, "No web bundle is installed on this server");
        };
        return match safe_static_path(root, &req.path) {
            Some(path) => Route::Static(path),
            None => reject(404, "Not found"),
        };
    }

    if req.header("sec-websocket-version") != Some("13") {
        return reject(400, "Unsupported WebSocket version");
    }
    let Some(key) = req.header("sec-websocket-key") else {
        return reject(400, "Sec-WebSocket-Key required");
    };
    // Every browser sends Origin on an upgrade and no page can forge it.
    // Without this check any site the user visits could open our socket:
    // WebSocket handshakes are not subject to the same-origin policy.
    match req.header("origin") {
        Some(origin) if site.origin_allowed(origin, req.header("host")) => {}
        Some(_) => return reject(403, "Origin not allowed"),
        None => return reject(403, "Origin required"),
    }
    let protocols = req.header_list("sec-websocket-protocol");
    let relay = !protocols.iter().any(|p| p == SUBPROTOCOL);
    if relay && !protocols.iter().any(|p| p == RELAY_SUBPROTOCOL) {
        return reject(400, "Unknown WebSocket subprotocol");
    }
    let mut tokens = protocols
        .iter()
        .filter_map(|p| p.strip_prefix(TOKEN_PROTOCOL_PREFIX));
    match (tokens.next(), tokens.next()) {
        (Some(token), None) if !token.is_empty() => Route::Upgrade {
            key: key.to_string(),
            token: token.to_string(),
            relay,
        },
        _ => reject(401, "Web token required"),
    }
}

/// RFC 6455 §4.2.2: the key the client sent, the GUID, sha1, base64.
pub fn accept_key(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WS_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

/// Resolve a URL path inside the bundle directory. `/` is the page; every
/// segment must be a plain file name, so nothing outside `root` can be
/// named however the path is spelled.
pub fn safe_static_path(root: &Path, url_path: &str) -> Option<PathBuf> {
    // The precompressed siblings are an implementation detail of how a
    // file is sent, not a second name for it: asking for one directly
    // would hand back gzip bytes with no Content-Encoding to say so.
    if url_path.ends_with(".gz") {
        return None;
    }
    let path = url_path.split(['?', '#']).next().unwrap_or("");
    let path = if path == "/" || path.is_empty() {
        "/index.html"
    } else {
        path
    };
    let mut resolved = root.to_path_buf();
    for segment in path.strip_prefix('/').unwrap_or(path).split('/') {
        let plain = !segment.is_empty()
            && !segment.starts_with('.')
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !plain {
            return None;
        }
        resolved.push(segment);
    }
    Some(resolved)
}

pub fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "woff2" => "font/woff2",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn response_head(
    status: u16,
    reason: &str,
    extra: &[(&str, &str)],
    body_len: Option<usize>,
) -> Vec<u8> {
    // `frame-ancestors` and its older spelling: a page that opens a shell
    // has no business inside someone else's frame. Deliberately not a whole
    // Content-Security-Policy -- `script-src` and `style-src` rules are how
    // one breaks a wasm and WebGPU page, and framing is the gap that was
    // actually open.
    let mut out = format!(
        "HTTP/1.1 {status} {reason}\r\nConnection: close\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: frame-ancestors 'none'\r\nX-Frame-Options: DENY\r\n"
    );
    for (name, value) in extra {
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }
    match body_len {
        Some(len) => out.push_str(&format!("Content-Length: {len}\r\n\r\n")),
        // A 304 carries no body and no length for one.
        None => out.push_str("\r\n"),
    }
    out.into_bytes()
}

/// Write `bytes` a piece at a time, each within `WRITE_STALL`. `Err` means
/// the peer stopped taking them; the caller gives up on it.
async fn write_within<S: AsyncWrite + Unpin>(stream: &mut S, bytes: &[u8]) -> Result<(), ()> {
    for chunk in bytes.chunks(WRITE_CHUNK) {
        let written = smol::future::or(
            async { stream.write_all(chunk).await.is_ok() },
            async {
                smol::Timer::after(WRITE_STALL).await;
                false
            },
        )
        .await;
        if !written {
            return Err(());
        }
    }
    Ok(())
}

/// Flush and close, neither for longer than a stalled write is allowed.
async fn finish<S: AsyncWrite + Unpin>(stream: &mut S) {
    smol::future::or(
        async {
            let _ = stream.flush().await;
            let _ = stream.close().await;
        },
        async {
            smol::Timer::after(WRITE_STALL).await;
        },
    )
    .await;
}

async fn respond<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    reason: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) {
    let mut bytes = response_head(status, reason, extra, Some(body.len()));
    bytes.extend_from_slice(body);
    if write_within(stream, &bytes).await.is_ok() {
        finish(stream).await;
    }
}

/// The weight this request gives a coding, taking the wildcard into
/// account: `gzip;q=0.8`, `*;q=0`, or plain `gzip` (which means 1). None
/// means the coding was not mentioned and no wildcard covered it.
fn encoding_weight(req: &Request, coding: &str) -> Option<f32> {
    let mut wildcard = None;
    for item in req.header_list("accept-encoding") {
        let mut parts = item.split(';');
        let name = parts.next().unwrap_or("").trim();
        // A weight we cannot read is not a refusal: a truncated header or
        // a proxy that rewrote it should not cost the client its bytes.
        let mut q = 1.0f32;
        for param in parts {
            let param = param.trim();
            if let Some(value) = param.strip_prefix("q=").or_else(|| param.strip_prefix("Q=")) {
                q = value.trim().parse().unwrap_or(1.0);
            }
        }
        if name.eq_ignore_ascii_case(coding) {
            return Some(q);
        }
        if name == "*" && wildcard.is_none() {
            wildcard = Some(q);
        }
    }
    wildcard
}

fn accepts_gzip(req: &Request) -> bool {
    encoding_weight(req, "gzip")
        .map(|q| q > 0.0)
        .unwrap_or(false)
}

/// A validator for the exact bytes being sent: the length and modification
/// time of the file that carries them, which the response already had to
/// read. The compressed sibling is a different file with its own pair, so
/// the two representations never share a tag.
fn etag(len: u64, mtime: Option<SystemTime>) -> Option<String> {
    let stamp = mtime?.duration_since(UNIX_EPOCH).ok()?.as_nanos();
    Some(format!("\"{len:x}-{stamp:x}\""))
}

/// Whether `If-None-Match` says the client already holds these bytes.
fn already_held(header: Option<&str>, tag: &str) -> bool {
    header
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .any(|item| {
            item == "*" || item == tag || item.strip_prefix("W/").map(str::trim) == Some(tag)
        })
}

/// The uncompressed length gzip records in its last four bytes.
///
/// This is a length, not a digest: it is taken modulo 2^32 and says
/// nothing about the content, so an edit that keeps the size passes it.
/// It answers the question modification times cannot -- whether a copy
/// that rewrote the timestamps left a sibling standing for other bytes --
/// and the time comparison answers the one this cannot. Both are cheap,
/// and each catches what the other misses.
async fn gzip_uncompressed_len(path: &Path) -> Option<u64> {
    let mut file = smol::fs::File::open(path).await.ok()?;
    file.seek(SeekFrom::End(-4)).await.ok()?;
    let mut trailer = [0u8; 4];
    file.read_exact(&mut trailer).await.ok()?;
    Some(u32::from_le_bytes(trailer) as u64)
}

/// The `<name>.gz` beside a bundle file, when it is no older than the file
/// it stands for. `ci/build-web.sh` writes these, so serving one costs the
/// same as serving any other file: a chunk at a time, nothing held.
///
/// A sibling is used only when two things agree that it stands for the
/// file beside it: it is no older, and the length in its trailer is the
/// length of that file. Either alone can be fooled -- a copy that rewrites
/// timestamps defeats the first, an edit that keeps the size defeats the
/// second -- and serving the previous build compressed is worse than
/// serving this one plainly, so a sibling that fails either is ignored.
async fn precompressed(path: &Path) -> Option<PathBuf> {
    let mut name = path.as_os_str().to_os_string();
    name.push(".gz");
    let packed = PathBuf::from(name);
    let meta = smol::fs::metadata(&packed).await.ok()?;
    if !meta.is_file() {
        return None;
    }
    let plain = smol::fs::metadata(path).await.ok()?;
    if meta.modified().ok()? < plain.modified().ok()? {
        return None;
    }
    // The trailer counts modulo 2^32, so compare the file's length the
    // same way rather than refusing everything past 4 GiB.
    let wrapped = plain.len() % (1u64 << 32);
    (gzip_uncompressed_len(&packed).await? == wrapped).then_some(packed)
}

/// Send a file from the bundle without holding more than one piece of it:
/// a slow reader of the wasm costs a seat, not megabytes.
///
/// When the client takes gzip and a precompressed sibling is there, that
/// file is sent instead, with the content type of the name that was asked
/// for. Compression itself happens at build time; nothing here does it.
async fn send_file<S: AsyncWrite + Unpin>(
    stream: &mut S,
    path: &Path,
    gzip_ok: bool,
    if_none_match: Option<&str>,
) {
    let packed = if gzip_ok { precompressed(path).await } else { None };
    let sending = packed.as_deref().unwrap_or(path);
    let file = match smol::fs::File::open(sending).await {
        Ok(file) => file,
        Err(_) => {
            return respond(
                stream,
                404,
                "Not Found",
                &[("Content-Type", "text/plain; charset=utf-8")],
                b"Not found",
            )
            .await;
        }
    };
    let (len, mtime) = match file.metadata().await {
        Ok(meta) if meta.is_file() => (meta.len(), meta.modified().ok()),
        _ => {
            return respond(
                stream,
                404,
                "Not Found",
                &[("Content-Type", "text/plain; charset=utf-8")],
                b"Not found",
            )
            .await;
        }
    };
    // A reload asks whether what it holds is still current; when it is,
    // that is a few dozen bytes instead of the whole bundle again.
    let tag = etag(len, mtime);
    if let Some(tag) = &tag {
        if already_held(if_none_match, tag) {
            let head = response_head(
                304,
                "Not Modified",
                &[
                    ("ETag", tag),
                    ("Vary", "Accept-Encoding"),
                    ("Cache-Control", "no-cache"),
                ],
                None,
            );
            if write_within(stream, &head).await.is_ok() {
                finish(stream).await;
            }
            return;
        }
    }
    let mut extra = vec![
        // The type of what was asked for, not of the .gz that carries it.
        ("Content-Type", content_type(path)),
        ("Vary", "Accept-Encoding"),
        ("Cache-Control", "no-cache"),
    ];
    if packed.is_some() {
        extra.push(("Content-Encoding", "gzip"));
    }
    if let Some(tag) = &tag {
        extra.push(("ETag", tag));
    }
    let head = response_head(200, "OK", &extra, Some(len as usize));
    if write_within(stream, &head).await.is_err() {
        return;
    }
    let mut file = file;
    let mut buf = vec![0u8; WRITE_CHUNK];
    let mut sent = 0u64;
    while sent < len {
        let want = ((len - sent) as usize).min(buf.len());
        let n = match file.read(&mut buf[..want]).await {
            // A file that shrank under us: the browser sees a short body
            // and retries; nothing better can be said mid-response.
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if write_within(stream, &buf[..n]).await.is_err() {
            return;
        }
        sent += n as u64;
    }
    finish(stream).await;
}

/// One accepted socket, from the first byte of the request to the end of
/// the response -- or, for an admitted WebSocket, the end of the session.
pub async fn serve<S>(mut stream: S, site: Arc<WebSite>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let Some(seat) = PreAuthSeat::take() else {
        respond(&mut stream, 503, "Service Unavailable", &[], b"").await;
        return;
    };
    serve_seated(stream, site, seat).await
}

/// `serve` for a connection whose seat was taken earlier (before a TLS
/// handshake).
pub async fn serve_seated<S>(mut stream: S, site: Arc<WebSite>, seat: PreAuthSeat)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut head = Vec::with_capacity(1024);
    let mut buf = [0u8; 4096];
    let mut deadline = smol::Timer::after(HEAD_TIMEOUT);
    let (request, head_len) = loop {
        let read = smol::future::or(
            async { Some(stream.read(&mut buf).await) },
            async {
                (&mut deadline).await;
                None
            },
        )
        .await;
        match read {
            None => {
                respond(&mut stream, 408, "Request Timeout", &[], b"").await;
                return;
            }
            Some(Ok(0)) => return,
            Some(Err(err)) => {
                log::debug!("web request read failed: {err}");
                return;
            }
            Some(Ok(n)) => head.extend_from_slice(&buf[..n]),
        }
        match parse_head(&head) {
            Ok(Some(parsed)) => break parsed,
            Ok(None) if head.len() > MAX_HEAD => {
                respond(&mut stream, 431, "Request Header Fields Too Large", &[], b"").await;
                return;
            }
            Ok(None) => continue,
            Err(err) => {
                log::debug!("web request rejected: {err}");
                respond(&mut stream, 400, "Bad Request", &[], b"").await;
                return;
            }
        }
    };

    match route(&site, &request) {
        Route::Reject { status, reason } => {
            log::debug!(
                "web request {} from origin {:?} rejected: {status} {reason}",
                describe(&request),
                request.header("origin")
            );
            respond(
                &mut stream,
                status,
                reason,
                &[("Content-Type", "text/plain; charset=utf-8")],
                reason.as_bytes(),
            )
            .await;
        }
        Route::Static(path) => {
            send_file(
                &mut stream,
                &path,
                accepts_gzip(&request),
                request.header("if-none-match"),
            )
            .await
        }
        Route::Upgrade { key, token, relay } => {
            // Taken here and nowhere else: the header belongs to the
            // connection being admitted, and there is no second chance to
            // read it once the socket becomes a mux stream.
            let device = request
                .header("user-agent")
                .and_then(crate::web_auth::describe_user_agent);
            let Some(admission) = WEB_TOKENS.verify(&token, device) else {
                log::warn!(
                    "web socket from origin {:?} refused: unknown or expired token",
                    request.header("origin")
                );
                respond(
                    &mut stream,
                    401,
                    "Unauthorized",
                    &[("Content-Type", "text/plain; charset=utf-8")],
                    b"Web token unknown or expired",
                )
                .await;
                return;
            };
            // Admitted: the seat is for the unauthenticated wait only.
            drop(seat);
            let accept = accept_key(&key);
            let protocol = if relay { RELAY_SUBPROTOCOL } else { SUBPROTOCOL };
            let reply = format!(
                "HTTP/1.1 101 Switching Protocols\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Accept: {accept}\r\n\
                 Sec-WebSocket-Protocol: {protocol}\r\n\r\n"
            );
            if let Err(err) = stream.write_all(reply.as_bytes()).await {
                log::debug!("web socket upgrade reply failed: {err}");
                return;
            }
            let _ = stream.flush().await;
            let leftover = head[head_len..].to_vec();
            if relay {
                log::info!(
                    "web relay admitted on {} with token {}",
                    site.describe,
                    admission.token_id
                );
                crate::web_relay::serve(Prefixed::new(leftover, stream), admission.revoked.clone())
                    .await;
                drop(admission);
                return;
            }
            // The token id when the link has no name of its own: a peer
            // has to be called something in `list-clients`, and the id is
            // the one handle that is always there and always unique.
            let peer_label = admission
                .label
                .clone()
                .unwrap_or_else(|| admission.token_id.clone());
            log::info!(
                "web client admitted on {} with token {} ({peer_label})",
                site.describe,
                admission.token_id,
            );
            let peer = ConnectionPeer::Web(WebPeer {
                token_id: admission.token_id.clone(),
                label: peer_label,
                username: site.username.clone(),
                revoked: admission.revoked.clone(),
            });
            let ws = WebStream::new(Prefixed::new(leftover, stream));
            if let Err(err) = process_stream(ws, peer).await {
                log::error!("web client connection ended: {err:#}");
            }
            log::info!(
                "web client with token {} disconnected",
                admission.token_id
            );
            drop(admission);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site() -> WebSite {
        WebSite::from_config(&config::WebServer::default(), Some("/srv/web".into()), "me".into())
    }

    fn request(method: &str, path: &str, headers: &[(&str, &str)]) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_ascii_lowercase(), v.to_string()))
                .collect(),
        }
    }

    fn upgrade_headers<'a>(origin: &'a str, protocols: &'a str) -> Vec<(&'a str, &'a str)> {
        vec![
            ("Host", "127.0.0.1:8088"),
            ("Connection", "keep-alive, Upgrade"),
            ("Upgrade", "websocket"),
            ("Sec-WebSocket-Version", "13"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("Origin", origin),
            ("Sec-WebSocket-Protocol", protocols),
        ]
    }

    fn accept_encoding(value: &str) -> Request {
        Request {
            method: "GET".into(),
            path: "/app.js".into(),
            headers: vec![("accept-encoding".into(), value.into())],
        }
    }

    #[test]
    fn if_none_match_accepts_the_shapes_a_client_sends() {
        let tag = "\"2a-17b\"";
        assert!(already_held(Some(tag), tag));
        assert!(already_held(Some("*"), tag));
        // A list, and the weak form a proxy may have rewritten it into.
        assert!(already_held(Some("\"other\", \"2a-17b\""), tag));
        assert!(already_held(Some("W/\"2a-17b\""), tag));
        assert!(!already_held(Some("\"other\""), tag));
        assert!(!already_held(None, tag));
        assert!(!already_held(Some(""), tag));
    }

    #[test]
    fn an_etag_follows_the_bytes_it_stands_for() {
        let now = SystemTime::now();
        let a = etag(100, Some(now)).unwrap();
        assert_eq!(etag(100, Some(now)).unwrap(), a, "same bytes, same tag");
        assert_ne!(etag(101, Some(now)).unwrap(), a, "a different length");
        assert_ne!(
            etag(100, Some(now + Duration::from_secs(1))).unwrap(),
            a,
            "a different modification time"
        );
        assert!(a.starts_with('"') && a.ends_with('"'), "{}", a);
        assert!(etag(100, None).is_none());
    }

    #[test]
    fn accept_encoding_reads_weights_and_the_wildcard() {
        assert!(accepts_gzip(&accept_encoding("gzip, deflate, br")));
        assert!(accepts_gzip(&accept_encoding("gzip;q=1")));
        assert!(accepts_gzip(&accept_encoding("GZIP;q=0.8")));
        assert!(accepts_gzip(&accept_encoding("*")));
        // A weight of zero is a refusal, and a named coding beats the
        // wildcard whichever way round they are written.
        assert!(!accepts_gzip(&accept_encoding("gzip;q=0")));
        assert!(!accepts_gzip(&accept_encoding("deflate, *;q=0")));
        assert!(accepts_gzip(&accept_encoding("*;q=0, gzip")));
        assert!(!accepts_gzip(&accept_encoding("br")));
        // No header at all, and an empty one, mean the bytes as they are.
        assert!(!accepts_gzip(&Request {
            method: "GET".into(),
            path: "/app.js".into(),
            headers: vec![],
        }));
        assert!(!accepts_gzip(&accept_encoding("")));
        // A weight we cannot read must not be taken for a refusal: a
        // truncated header should not cost the client its compression.
        assert!(accepts_gzip(&accept_encoding("gzip;q=")));
        assert!(accepts_gzip(&accept_encoding("gzip;q=high")));
        // Duplicates: the first mention wins, either way round.
        assert!(!accepts_gzip(&accept_encoding("gzip;q=0, gzip;q=1")));
        assert!(accepts_gzip(&accept_encoding("gzip;q=1, gzip;q=0")));
        // Two wildcards: the first also wins, so this is not a refusal.
        assert!(accepts_gzip(&accept_encoding("*;q=1, *;q=0")));
    }

    #[test]
    fn the_rfc_sample_key_produces_the_rfc_sample_accept() {
        // RFC 6455 section 1.3.
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn a_complete_head_parses_and_reports_its_length() {
        let bytes = b"GET /index.html HTTP/1.1\r\nHost: localhost:8088\r\nX-A: 1, 2\r\nX-A: 3\r\n\r\nrest";
        let (req, len) = parse_head(bytes).unwrap().expect("complete");
        assert_eq!(len, bytes.len() - 4);
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/index.html");
        assert_eq!(req.header("host"), Some("localhost:8088"));
        assert_eq!(req.header_list("x-a"), vec!["1", "2", "3"]);
        assert!(parse_head(b"GET / HTTP/1.1\r\nHost: x").unwrap().is_none());
        assert!(parse_head(b"\x00\x01 garbage").is_err());
    }

    #[test]
    fn static_paths_stay_inside_the_bundle() {
        let root = Path::new("/srv/web");
        assert_eq!(safe_static_path(root, "/"), Some("/srv/web/index.html".into()));
        assert_eq!(
            safe_static_path(root, "/pkg/app.js?v=1"),
            Some("/srv/web/pkg/app.js".into())
        );
        assert_eq!(safe_static_path(root, "/../etc/passwd"), None);
        assert_eq!(safe_static_path(root, "/pkg/../../x"), None);
        assert_eq!(safe_static_path(root, "/.hidden"), None);
        assert_eq!(safe_static_path(root, "//x"), None);
        assert_eq!(safe_static_path(root, "/a%2fb"), None);
    }

    #[test]
    fn plain_requests_are_static_and_need_a_known_host() {
        let site = site();
        let ok = request("GET", "/", &[("Host", "localhost:8088")]);
        assert_eq!(route(&site, &ok), Route::Static("/srv/web/index.html".into()));
        let ip = request("GET", "/", &[("Host", "127.0.0.1:8088")]);
        assert!(matches!(route(&site, &ip), Route::Static(_)));
        let rebound = request("GET", "/", &[("Host", "evil.example:8088")]);
        assert_eq!(route(&site, &rebound), reject(403, "Host not served here"));
        let wrong_port = request("GET", "/", &[("Host", "192.0.2.5:9999")]);
        assert_eq!(route(&site, &wrong_port), reject(403, "Host not served here"));
        // Through `ssh -L 9000:127.0.0.1:8088`: a loopback name on a port
        // the listener never heard of is still this machine.
        let forwarded = request("GET", "/", &[("Host", "localhost:9000")]);
        assert!(matches!(route(&site, &forwarded), Route::Static(_)));
        let bare = request("GET", "/", &[("Host", "localhost")]);
        assert!(matches!(route(&site, &bare), Route::Static(_)));
        let junk = request("GET", "/", &[("Host", "local host")]);
        assert_eq!(route(&site, &junk), reject(403, "Host not served here"));
        let post = request("POST", "/", &[("Host", "localhost:8088")]);
        assert_eq!(route(&site, &post), reject(405, "Method not allowed"));
        let nohost = request("GET", "/", &[]);
        assert_eq!(route(&site, &nohost), reject(400, "Host header required"));
        let two_hosts = request("GET", "/", &[("Host", "localhost:8088"), ("Host", "evil.example")]);
        assert_eq!(route(&site, &two_hosts), reject(400, "One Host header, please"));
        let v6 = request("GET", "/", &[("Host", "[::1]:8088")]);
        assert!(matches!(route(&site, &v6), Route::Static(_)));
    }

    #[test]
    fn an_upgrade_needs_our_origin_our_subprotocol_and_one_token() {
        let site = site();
        let good = request(
            "GET",
            "/ws",
            &upgrade_headers("http://localhost:8088", "thinkterm.v1, tt-token.abc"),
        );
        assert_eq!(
            route(&site, &good),
            Route::Upgrade {
                key: "dGhlIHNhbXBsZSBub25jZQ==".into(),
                token: "abc".into(),
                relay: false,
            }
        );
        let relay = request(
            "GET",
            "/ws",
            &upgrade_headers("http://localhost:8088", "thinkterm.relay.v1, tt-token.abc"),
        );
        assert!(matches!(route(&site, &relay), Route::Upgrade { relay: true, .. }));
        let foreign = request(
            "GET",
            "/ws",
            &upgrade_headers("https://evil.example", "thinkterm.v1, tt-token.abc"),
        );
        assert_eq!(route(&site, &foreign), reject(403, "Origin not allowed"));
        let no_token = request("GET", "/ws", &upgrade_headers("http://localhost:8088", "thinkterm.v1"));
        assert_eq!(route(&site, &no_token), reject(401, "Web token required"));
        let two_tokens = request(
            "GET",
            "/ws",
            &upgrade_headers("http://localhost:8088", "thinkterm.v1, tt-token.a, tt-token.b"),
        );
        assert_eq!(route(&site, &two_tokens), reject(401, "Web token required"));
        let other_proto = request("GET", "/ws", &upgrade_headers("http://localhost:8088", "tt-token.abc"));
        assert_eq!(route(&site, &other_proto), reject(400, "Unknown WebSocket subprotocol"));
        let mut headers = upgrade_headers("http://localhost:8088", "thinkterm.v1, tt-token.abc");
        headers.retain(|(n, _)| *n != "Origin");
        let no_origin = request("GET", "/ws", &headers);
        assert_eq!(route(&site, &no_origin), reject(403, "Origin required"));
    }

    /// The page a loopback listener served through a forward comes back
    /// on the forward's port: its origin is that port, on our scheme, at
    /// the authority the request names. Another local port's page is not.
    #[test]
    fn a_forwarded_loopback_page_may_open_the_socket() {
        let site = site();
        let mut headers = upgrade_headers("http://localhost:9000", "thinkterm.v1, tt-token.abc");
        headers.retain(|(n, _)| *n != "Host");
        headers.push(("Host", "localhost:9000"));
        let good = request("GET", "/ws", &headers);
        assert!(matches!(route(&site, &good), Route::Upgrade { .. }));
        // A different local port than the one addressed: not the page we served.
        let other = request("GET", "/ws", &upgrade_headers("http://localhost:9000", "thinkterm.v1, tt-token.abc"));
        assert_eq!(route(&site, &other), reject(403, "Origin not allowed"));
        let mut headers = upgrade_headers("https://localhost:9000", "thinkterm.v1, tt-token.abc");
        headers.retain(|(n, _)| *n != "Host");
        headers.push(("Host", "localhost:9000"));
        let wrong_scheme = request("GET", "/ws", &headers);
        assert_eq!(route(&site, &wrong_scheme), reject(403, "Origin not allowed"));
    }

    /// Every response, not just the page: an error body or a 304 lands in a
    /// frame just as well as the page does.
    #[test]
    fn no_response_may_be_framed() {
        for (status, reason, body_len) in
            [(200u16, "OK", Some(3usize)), (403, "Forbidden", Some(0)), (304, "Not Modified", None)]
        {
            let head = String::from_utf8(response_head(status, reason, &[], body_len)).unwrap();
            assert!(
                head.contains("\r\nContent-Security-Policy: frame-ancestors 'none'\r\n"),
                "{} carries no frame-ancestors: {:?}",
                status,
                head
            );
            assert!(
                head.contains("\r\nX-Frame-Options: DENY\r\n"),
                "{} carries no X-Frame-Options: {:?}",
                status,
                head
            );
        }
    }

    /// Same-origin is the rule, and `0.0.0.0` is the shape that proves it.
    /// A listener bound to one address *derives* that address as an allowed
    /// origin, so a test written the obvious way is answered by the derived
    /// list and never reaches the comparison -- it would pass whatever the
    /// comparison said. TEST-NET addresses cannot be among this machine's
    /// own, so these do reach it.
    #[test]
    fn a_page_this_listener_served_may_open_the_socket_and_no_other() {
        fn upgrade(site: &WebSite, host: &str, origin: &str) -> Route {
            let mut headers = upgrade_headers(origin, "thinkterm.v1, tt-token.abc");
            headers.retain(|(n, _)| *n != "Host");
            headers.push(("Host", host));
            route(site, &request("GET", "/ws", &headers))
        }
        fn listener(server: config::WebServer) -> WebSite {
            WebSite::from_config(&server, Some("/srv/web".into()), "me".into())
        }

        let anywhere = listener(config::WebServer {
            bind_address: "0.0.0.0:8088".into(),
            ..config::WebServer::default()
        });
        // The page, reached by an address that is not in this machine's own
        // list: a NAT, or a tailnet joined after the listener came up.
        assert!(matches!(
            upgrade(&anywhere, "203.0.113.5:8088", "http://203.0.113.5:8088"),
            Route::Upgrade { .. }
        ));
        // The fix itself: a page on an unrelated address, carrying our port.
        // The old rule admitted this on the strength of the origin being an
        // IP literal, whatever `Host` said.
        assert_eq!(
            upgrade(&anywhere, "198.51.100.7:8088", "http://203.0.113.5:8088"),
            reject(403, "Origin not allowed")
        );

        // A loopback listener reached through a forward bound to a real
        // address: `ssh -L 203.0.113.5:8088:127.0.0.1:8088`. Allowed before
        // and allowed now -- this is the case the old rule existed for.
        assert!(matches!(
            upgrade(&site(), "203.0.113.5:8088", "http://203.0.113.5:8088"),
            Route::Upgrade { .. }
        ));

        // A default port is spelled by leaving it out, in both headers, and
        // both sides have to fill it in the same way to agree.
        let secure = listener(config::WebServer {
            bind_address: "0.0.0.0:443".into(),
            pem_cert: Some("/c.pem".into()),
            pem_private_key: Some("/k.pem".into()),
            ..config::WebServer::default()
        });
        assert!(matches!(
            upgrade(&secure, "203.0.113.5", "https://203.0.113.5"),
            Route::Upgrade { .. }
        ));
        // The scheme is part of an origin.
        assert_eq!(
            upgrade(&secure, "203.0.113.5", "http://203.0.113.5:443"),
            reject(403, "Origin not allowed")
        );

        // With origins configured, that list is the whole answer: the page
        // served from this very port is not automatically one of them.
        let configured = listener(config::WebServer {
            allowed_origins: vec!["https://app.example.net".into()],
            ..config::WebServer::default()
        });
        assert_eq!(
            upgrade(&configured, "127.0.0.1:8088", "http://127.0.0.1:8088"),
            reject(403, "Origin not allowed")
        );
    }

    /// A listener on a scheme's default port is named without it, which
    /// is how a browser spells it.
    #[test]
    fn default_ports_match_with_or_without_the_number() {
        let server = config::WebServer {
            bind_address: "mux.example.net:443".into(),
            pem_cert: Some("/c.pem".into()),
            pem_private_key: Some("/k.pem".into()),
            ..config::WebServer::default()
        };
        let site = WebSite::from_config(&server, Some("/srv/web".into()), "me".into());
        for host in ["mux.example.net", "mux.example.net:443", "MUX.example.net"] {
            let req = request("GET", "/", &[("Host", host)]);
            assert!(matches!(route(&site, &req), Route::Static(_)), "{}", host);
        }
        let odd = request("GET", "/", &[("Host", "mux.example.net:8443")]);
        assert_eq!(route(&site, &odd), reject(403, "Host not served here"));
        let mut headers = upgrade_headers("https://mux.example.net", "thinkterm.v1, tt-token.abc");
        headers.retain(|(n, _)| *n != "Host");
        headers.push(("Host", "mux.example.net"));
        assert!(matches!(route(&site, &request("GET", "/ws", &headers)), Route::Upgrade { .. }));
        // Not loopback: a stranger's port is not ours.
        let mut headers = upgrade_headers("https://mux.example.net:9000", "thinkterm.v1, tt-token.abc");
        headers.retain(|(n, _)| *n != "Host");
        headers.push(("Host", "mux.example.net:9000"));
        assert_eq!(route(&site, &request("GET", "/ws", &headers)), reject(403, "Host not served here"));
    }

    #[test]
    fn a_configured_origin_also_names_an_allowed_host() {
        let server = config::WebServer {
            allowed_origins: vec!["https://app.example.net".into()],
            ..config::WebServer::default()
        };
        let site = WebSite::from_config(&server, None, "me".into());
        let req = request("GET", "/", &[("Host", "app.example.net")]);
        assert_eq!(route(&site, &req), reject(404, "No web bundle is installed on this server"));
    }
}

#[cfg(all(test, unix))]
mod end_to_end {
    use super::*;
    use codec::Pdu;
    use mux::Mux;
    use smol::Async;
    use std::os::fd::{FromRawFd, IntoRawFd};
    use std::os::unix::net::UnixStream;

    fn pair() -> (Async<UnixStream>, Async<UnixStream>) {
        let (a, b) = UnixStream::pair().unwrap();
        let a = unsafe { UnixStream::from_raw_fd(a.into_raw_fd()) };
        let b = unsafe { UnixStream::from_raw_fd(b.into_raw_fd()) };
        (Async::new(a).unwrap(), Async::new(b).unwrap())
    }

    async fn read_response(client: &mut Async<UnixStream>) -> String {
        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = client.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            if out.windows(4).any(|w| w == b"\r\n\r\n") && out.starts_with(b"HTTP/1.1 101") {
                break;
            }
        }
        String::from_utf8_lossy(&out).to_string()
    }

    async fn read_bytes(client: &mut Async<UnixStream>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match client.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
        }
        out
    }

    fn split_response(bytes: &[u8]) -> (String, Vec<u8>) {
        let end = bytes
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("response head");
        (
            String::from_utf8_lossy(&bytes[..end]).to_string(),
            bytes[end + 4..].to_vec(),
        )
    }

    fn bundle_with(name: &str, body: &[u8]) -> (tempfile::TempDir, Arc<WebSite>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(name), body).unwrap();
        let site = Arc::new(WebSite::from_config(
            &config::WebServer::default(),
            Some(dir.path().to_path_buf()),
            "me".into(),
        ));
        (dir, site)
    }

    /// gzip a file the way `ci/build-web.sh` does, beside the original.
    fn write_sibling(dir: &Path, name: &str, body: &[u8]) {
        std::fs::write(dir.join(name), body).unwrap();
        let packed = std::process::Command::new("gzip")
            .args(["-9", "-c"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(body)?;
                child.wait_with_output()
            })
            .unwrap();
        std::fs::write(dir.join(format!("{name}.gz")), packed.stdout).unwrap();
    }

    #[test]
    fn a_client_that_accepts_gzip_is_sent_the_precompressed_sibling() {
        let body = "console.log('the bundle');\n".repeat(500).into_bytes();
        let dir = tempfile::tempdir().unwrap();
        write_sibling(dir.path(), "app.js", &body);
        let site = Arc::new(WebSite::from_config(
            &config::WebServer::default(),
            Some(dir.path().to_path_buf()),
            "me".into(),
        ));
        let (server, mut client) = pair();
        crate::connections::spawn(serve(server, site));
        smol::block_on(async {
            let req = "GET /app.js HTTP/1.1\r\nHost: localhost:8088\r\n\
                       Accept-Encoding: gzip, deflate\r\n\r\n";
            client.write_all(req.as_bytes()).await.unwrap();
            let (head, packed) = split_response(&read_bytes(&mut client).await);
            assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
            assert!(head.contains("Content-Encoding: gzip"), "{}", head);
            // Without this a shared cache could hand the compressed body to
            // a client that never asked for one.
            assert!(head.contains("Vary: Accept-Encoding"), "{}", head);
            // The type of the name that was asked for, not of the .gz.
            assert!(head.contains("Content-Type: text/javascript"), "{}", head);
            assert!(
                packed.len() < body.len() / 2,
                "{} bytes is no better than {}",
                packed.len(),
                body.len()
            );
            assert!(
                head.contains(&format!("Content-Length: {}", packed.len())),
                "{}",
                head
            );
            let mut unpacked = Vec::new();
            std::io::Read::read_to_end(
                &mut std::process::Command::new("gunzip")
                    .arg("-c")
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .map(|mut child| {
                        use std::io::Write;
                        child.stdin.take().unwrap().write_all(&packed).unwrap();
                        child.wait_with_output().unwrap().stdout
                    })
                    .unwrap()
                    .as_slice(),
                &mut unpacked,
            )
            .unwrap();
            assert_eq!(unpacked, body);
        });
    }

    #[test]
    fn a_client_that_says_nothing_gets_the_plain_file_even_when_a_sibling_exists() {
        let body = b"<!doctype html><title>x</title>".to_vec();
        let dir = tempfile::tempdir().unwrap();
        write_sibling(dir.path(), "index.html", &body);
        let site = Arc::new(WebSite::from_config(
            &config::WebServer::default(),
            Some(dir.path().to_path_buf()),
            "me".into(),
        ));
        let (server, mut client) = pair();
        crate::connections::spawn(serve(server, site));
        smol::block_on(async {
            let req = "GET /index.html HTTP/1.1\r\nHost: localhost:8088\r\n\r\n";
            client.write_all(req.as_bytes()).await.unwrap();
            let (head, sent) = split_response(&read_bytes(&mut client).await);
            assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
            assert!(!head.contains("Content-Encoding"), "{}", head);
            assert!(head.contains("Vary: Accept-Encoding"), "{}", head);
            assert_eq!(sent, body);
        });
    }

    #[test]
    fn a_reload_that_holds_the_current_bytes_is_answered_304() {
        let body = "the bundle\n".repeat(200).into_bytes();
        let dir = tempfile::tempdir().unwrap();
        write_sibling(dir.path(), "app.js", &body);
        let site = Arc::new(WebSite::from_config(
            &config::WebServer::default(),
            Some(dir.path().to_path_buf()),
            "me".into(),
        ));
        let get = |extra: String| {
            let site = Arc::clone(&site);
            let (server, mut client) = pair();
            crate::connections::spawn(serve(server, site));
            smol::block_on(async move {
                let req = format!(
                    "GET /app.js HTTP/1.1\r\nHost: localhost:8088\r\n\
                     Accept-Encoding: gzip\r\n{extra}\r\n"
                );
                client.write_all(req.as_bytes()).await.unwrap();
                split_response(&read_bytes(&mut client).await)
            })
        };

        let (head, first) = get(String::new());
        assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
        let tag = head
            .lines()
            .find_map(|l| l.strip_prefix("ETag: "))
            .expect("an ETag")
            .to_string();
        assert!(!first.is_empty());

        // The reload: same bytes, so no body and no length for one.
        let (head, body_again) = get(format!("If-None-Match: {tag}\r\n"));
        assert!(head.starts_with("HTTP/1.1 304"), "{}", head);
        assert!(body_again.is_empty(), "304 carried {} bytes", body_again.len());
        assert!(!head.contains("Content-Length"), "{}", head);
        assert!(head.contains(&format!("ETag: {tag}")), "{}", head);

        // A tag for something else still gets the file.
        let (head, sent) = get("If-None-Match: \"stale\"\r\n".to_string());
        assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
        assert_eq!(sent.len(), first.len());
    }

    #[test]
    fn the_two_representations_do_not_share_an_etag() {
        let dir = tempfile::tempdir().unwrap();
        write_sibling(dir.path(), "app.js", b"the same bytes either way\n");
        let site = Arc::new(WebSite::from_config(
            &config::WebServer::default(),
            Some(dir.path().to_path_buf()),
            "me".into(),
        ));
        let tag_for = |accept: &str| {
            let site = Arc::clone(&site);
            let (server, mut client) = pair();
            crate::connections::spawn(serve(server, site));
            let accept = accept.to_string();
            smol::block_on(async move {
                let req = format!(
                    "GET /app.js HTTP/1.1\r\nHost: localhost:8088\r\n{accept}\r\n"
                );
                client.write_all(req.as_bytes()).await.unwrap();
                let (head, _) = split_response(&read_bytes(&mut client).await);
                head.lines()
                    .find_map(|l| l.strip_prefix("ETag: "))
                    .map(str::to_string)
                    .expect("an ETag")
            })
        };
        // Handing a compressed body's tag back for the plain one -- or the
        // other way round -- must not read as "you already have it".
        assert_ne!(
            tag_for("Accept-Encoding: gzip\r\n"),
            tag_for(""),
            "gzip and identity shared a tag"
        );
    }

    #[test]
    fn the_precompressed_sibling_is_not_a_url_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        write_sibling(dir.path(), "app.js", b"the build\n");
        let site = Arc::new(WebSite::from_config(
            &config::WebServer::default(),
            Some(dir.path().to_path_buf()),
            "me".into(),
        ));
        let (server, mut client) = pair();
        crate::connections::spawn(serve(server, site));
        smol::block_on(async {
            let req = "GET /app.js.gz HTTP/1.1\r\nHost: localhost:8088\r\n\r\n";
            client.write_all(req.as_bytes()).await.unwrap();
            let (head, _) = split_response(&read_bytes(&mut client).await);
            assert!(head.starts_with("HTTP/1.1 404"), "{}", head);
        });
    }

    /// Copy a file the way the packaging scripts do, timestamps and all.
    fn copy_preserving_times(from: &Path, to: &Path) {
        let status = std::process::Command::new("cp")
            .arg("-p")
            .arg(from)
            .arg(to)
            .status()
            .unwrap();
        assert!(status.success(), "cp -p failed");
    }

    #[test]
    fn a_sibling_survives_the_copy_the_packaging_scripts_do() {
        let build = tempfile::tempdir().unwrap();
        write_sibling(build.path(), "app.js", b"the build\n");
        // ci/deploy.sh copies the plain file first and the sibling after,
        // which without -p would leave the sibling looking newer than it is
        // -- and the other order would leave it looking stale.
        let shipped = tempfile::tempdir().unwrap();
        copy_preserving_times(&build.path().join("app.js"), &shipped.path().join("app.js"));
        copy_preserving_times(
            &build.path().join("app.js.gz"),
            &shipped.path().join("app.js.gz"),
        );
        smol::block_on(async {
            assert!(
                precompressed(&shipped.path().join("app.js")).await.is_some(),
                "a packaged sibling was refused"
            );
        });
    }

    #[test]
    fn a_rebuild_that_kept_the_length_still_rejects_the_old_sibling() {
        let dir = tempfile::tempdir().unwrap();
        write_sibling(dir.path(), "app.js", b"version = \"1\"\n");
        // The same number of bytes, so the trailer still matches: only the
        // modification time can tell that the sibling is a build behind.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.path().join("app.js"), b"version = \"2\"\n").unwrap();
        let path = dir.path().join("app.js");
        smol::block_on(async {
            // The trailer still matches, so this proves the time comparison
            // is what rejects the sibling -- the length cannot.
            assert_eq!(
                gzip_uncompressed_len(&dir.path().join("app.js.gz"))
                    .await
                    .unwrap(),
                std::fs::metadata(&path).unwrap().len(),
                "this test only means something while the length agrees"
            );
            assert!(
                precompressed(&path).await.is_none(),
                "a same-length rebuild was served from the old sibling"
            );
        });
    }

    #[test]
    fn a_sibling_that_stands_for_other_bytes_is_refused_however_new_it_looks() {
        let dir = tempfile::tempdir().unwrap();
        // The sibling holds a previous, shorter build; the packaging copy
        // then gave it the newer timestamp, so only its trailer gives it
        // away.
        std::fs::write(dir.path().join("app.js"), b"the second build, longer\n").unwrap();
        let packed = std::process::Command::new("gzip")
            .args(["-9", "-n", "-c"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(b"the first build\n")?;
                child.wait_with_output()
            })
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.path().join("app.js.gz"), packed.stdout).unwrap();
        let path = dir.path().join("app.js");
        smol::block_on(async {
            assert!(
                precompressed(&path).await.is_none(),
                "a sibling for other bytes was trusted because it looked newer"
            );
        });
    }

    #[test]
    fn a_sibling_older_than_its_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        write_sibling(dir.path(), "app.js", b"the first build\n");
        // Rebuild the original without rerunning the script: the sibling now
        // holds the previous build and must not be served for it.
        std::fs::write(dir.path().join("app.js"), b"the second build\n").unwrap();
        let path = dir.path().join("app.js");
        smol::block_on(async {
            assert!(
                precompressed(&path).await.is_none(),
                "a stale sibling was trusted"
            );
        });
        // Regenerating it makes the sibling usable again.
        write_sibling(dir.path(), "app.js", b"the second build\n");
        smol::block_on(async {
            assert!(precompressed(&path).await.is_some());
        });
    }

    #[test]
    fn an_unauthenticated_socket_gets_no_pdu_answered() {
        let site = Arc::new(WebSite::from_config(&config::WebServer::default(), None, "me".into()));
        let (server, mut client) = pair();
        crate::connections::spawn(serve(server, site));
        smol::block_on(async {
            let req = "GET /ws HTTP/1.1\r\nHost: localhost:8088\r\nOrigin: http://localhost:8088\r\n\
                       Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
                       Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                       Sec-WebSocket-Protocol: thinkterm.v1, tt-token.nope\r\n\r\n";
            client.write_all(req.as_bytes()).await.unwrap();
            let response = read_response(&mut client).await;
            assert!(response.starts_with("HTTP/1.1 401"), "{}", response);
            // Nothing sent after the refusal is answered: the socket is gone.
            let mut bytes = Vec::new();
            Pdu::GetCodecVersion(codec::GetCodecVersion {})
                .encode(&mut bytes, 1)
                .unwrap();
            let _ = client.write_all(&bytes).await;
            let mut buf = [0u8; 64];
            let n = smol::future::or(
                async { client.read(&mut buf).await.unwrap_or(0) },
                async {
                    smol::Timer::after(Duration::from_secs(2)).await;
                    usize::MAX
                },
            )
            .await;
            assert_eq!(n, 0, "expected end of stream, got {n} bytes");
        });
    }

    #[test]
    fn a_good_token_reaches_dispatch_and_answers_the_version_handshake() {
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let minted = WEB_TOKENS.mint(Some("e2e".into()), None).unwrap();
        let site = Arc::new(WebSite::from_config(&config::WebServer::default(), None, "me".into()));
        let (server, mut client) = pair();
        crate::connections::spawn(serve(server, site));
        smol::block_on(async {
            let req = format!(
                "GET /ws HTTP/1.1\r\nHost: 127.0.0.1:8088\r\nOrigin: http://127.0.0.1:8088\r\n\
                 Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Protocol: thinkterm.v1, tt-token.{}\r\n\r\n",
                minted.token
            );
            client.write_all(req.as_bytes()).await.unwrap();
            let response = read_response(&mut client).await;
            assert!(response.starts_with("HTTP/1.1 101"), "{}", response);
            assert!(response.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
            assert!(response.contains("Sec-WebSocket-Protocol: thinkterm.v1"));

            let (mut tx, mut rx) =
                soketto::connection::Builder::new(client, soketto::connection::Mode::Client)
                    .finish();
            let mut bytes = Vec::new();
            Pdu::GetCodecVersion(codec::GetCodecVersion {})
                .encode(&mut bytes, 5)
                .unwrap();
            tx.send_binary(&bytes).await.unwrap();
            tx.flush().await.unwrap();
            let mut answer = Vec::new();
            rx.receive_data(&mut answer).await.unwrap();
            let decoded = Pdu::decode(std::io::Cursor::new(answer)).unwrap();
            assert_eq!(decoded.serial, 5);
            assert!(matches!(decoded.pdu, Pdu::GetCodecVersionResponse(_)));
        });
        WEB_TOKENS.revoke(Some(&minted.id));
    }

    #[test]
    fn the_page_is_served_from_the_bundle_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"<!doctype html>hello").unwrap();
        let site = Arc::new(WebSite::from_config(
            &config::WebServer::default(),
            Some(dir.path().to_path_buf()),
            "me".into(),
        ));
        let (server, mut client) = pair();
        crate::connections::spawn(serve(server, site));
        smol::block_on(async {
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost:8088\r\n\r\n")
                .await
                .unwrap();
            let response = read_response(&mut client).await;
            assert!(response.starts_with("HTTP/1.1 200"), "{}", response);
            assert!(response.contains("Content-Type: text/html"));
            assert!(response.ends_with("<!doctype html>hello"), "{}", response);
        });
    }
}
