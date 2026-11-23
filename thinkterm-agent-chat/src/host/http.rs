//! An HTTP/1.1 client for a service on this machine, which is how
//! OpenCode's listens: requests with a JSON body and their answers, and an
//! event stream that stays open. Blocking, one connection a request, no
//! TLS: it talks only to this machine's own addresses, so a password it
//! sends never crosses a network in the clear.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// The head of an answer may be no longer than this.
const MAX_HEAD: usize = 64 << 10;
/// A line that says how long a chunk is, or the end of one.
const MAX_CHUNK_LINE: usize = 4 << 10;
/// The largest answer taken in whole: a conversation's messages carry the
/// pictures sent in it.
const MAX_BODY: usize = 256 << 20;

/// Where a service listens, and what it is told of who asks.
#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// `host:port` as the URL names it (`127.0.0.1:4096`, `[::1]:4096`).
    pub authority: String,
    /// The path the service is mounted under (`/opencode` behind a proxy),
    /// or nothing; never ending in `/`.
    pub base: String,
    /// The `Authorization` header's value, when the service wants one.
    pub auth: Option<String>,
}

/// The password is never written out.
impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("authority", &self.authority)
            .field("base", &self.base)
            .field("auth", &self.auth.as_ref().map(|_| "…"))
            .finish()
    }
}

impl Endpoint {
    /// The service at `url` (`http://host:port[/path]`, its query left
    /// out); none for another kind of URL, `https` among them.
    pub fn parse(url: &str, auth: Option<String>) -> Option<Self> {
        let rest = url.trim().strip_prefix("http://")?;
        let rest = rest.split(['?', '#']).next()?;
        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, ""),
        };
        let authority = authority.trim();
        if authority.is_empty() || authority.contains('@') {
            return None;
        }
        // Said as it is in each request's first line.
        if path.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return None;
        }
        let base = path.trim_end_matches('/').to_string();
        // A URL without a port is on HTTP's own.
        let has_port = match authority.rfind(']') {
            Some(close) => authority[close..].contains(':'),
            None => authority.contains(':'),
        };
        let authority = if has_port {
            authority.to_string()
        } else {
            format!("{authority}:80")
        };
        Some(Self { authority, base, auth })
    }

    /// Basic authentication, as a header's value.
    pub fn basic(user: &str, password: &str) -> String {
        use base64::Engine;
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
        )
    }

    fn connect(&self, wait: Duration) -> io::Result<TcpStream> {
        // A service that listens on every address is reached on the loopback.
        let addresses: Vec<SocketAddr> = self
            .authority
            .to_socket_addrs()?
            .map(|mut address| {
                if address.ip().is_unspecified() {
                    address.set_ip(match address.ip() {
                        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
                    });
                }
                address
            })
            .collect();
        let own = own_addresses();
        let addresses: Vec<SocketAddr> = addresses
            .into_iter()
            .filter(|address| address.ip().is_loopback() || own.contains(&address.ip()))
            .collect();
        if addresses.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is not this machine: its password is not sent over a network in the clear", self.authority),
            ));
        }
        let mut last = io::Error::new(io::ErrorKind::NotFound, format!("no address for {}", self.authority));
        for address in addresses {
            match TcpStream::connect_timeout(&address, wait) {
                Ok(stream) => {
                    stream.set_nodelay(true)?;
                    return Ok(stream);
                }
                Err(err) => last = err,
            }
        }
        Err(last)
    }

    fn head(&self, method: &str, path: &str, accept: &str, body: Option<&[u8]>) -> Vec<u8> {
        let mut head = format!(
            "{method} {}{path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: thinkterm\r\nAccept: {accept}\r\nConnection: close\r\n",
            self.base, self.authority
        );
        if let Some(auth) = &self.auth {
            head.push_str(&format!("Authorization: {auth}\r\n"));
        }
        if let Some(body) = body {
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ));
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(body.unwrap_or_default());
        bytes
    }
}

/// An answer: its status, and its body whole.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Asks the service and waits for all of its answer, at most `wait` for
/// each step (connecting, and each read).
pub fn request(endpoint: &Endpoint, method: &str, path: &str, body: Option<&[u8]>, wait: Duration) -> io::Result<Response> {
    let mut stream = endpoint.connect(wait)?;
    stream.set_read_timeout(Some(wait))?;
    stream.set_write_timeout(Some(wait))?;
    stream.write_all(&endpoint.head(method, path, "application/json", body))?;
    let mut reader = BufReader::new(stream);
    let (status, headers) = read_head(&mut reader)?;
    let mut body = Vec::new();
    body_of(reader, &headers)
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut body)?;
    if body.len() > MAX_BODY {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "answer too long"));
    }
    Ok(Response { status, body })
}

/// Opens an event stream (`text/event-stream`): its body, read as it
/// comes. Reading fails once nothing at all came for `quiet` (a service
/// that sends heartbeats says something well within it).
pub fn stream(endpoint: &Endpoint, path: &str, wait: Duration, quiet: Duration) -> io::Result<(u16, Box<dyn BufRead + Send>)> {
    let mut stream = endpoint.connect(wait)?;
    stream.set_read_timeout(Some(wait))?;
    stream.set_write_timeout(Some(wait))?;
    stream.write_all(&endpoint.head("GET", path, "text/event-stream", None))?;
    let mut reader = BufReader::new(stream);
    let (status, headers) = read_head(&mut reader)?;
    reader.get_ref().set_read_timeout(Some(quiet))?;
    Ok((status, Box::new(BufReader::new(body_of(reader, &headers)))))
}

/// This machine's own addresses: those of its network interfaces.
fn own_addresses() -> Vec<IpAddr> {
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a list it allocates, read here
    // while it lives and given back to freeifaddrs once.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return found;
        }
        let mut at = list;
        while !at.is_null() {
            let entry = &*at;
            if !entry.ifa_addr.is_null() {
                match i32::from((*entry.ifa_addr).sa_family) {
                    libc::AF_INET => {
                        let v4 = &*(entry.ifa_addr as *const libc::sockaddr_in);
                        found.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr))));
                    }
                    libc::AF_INET6 => {
                        let v6 = &*(entry.ifa_addr as *const libc::sockaddr_in6);
                        found.push(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr)));
                    }
                    _ => {}
                }
            }
            at = entry.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    found
}

/// Reads to the end of a line, its `\n` kept, as `read_until` does; but
/// fails once more than `max` bytes came without one, so that an answer
/// that never ends a line cannot fill the memory. Zero at the end.
pub fn read_line_within(reader: &mut impl BufRead, line: &mut Vec<u8>, max: usize) -> io::Result<usize> {
    let mut read = 0;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if available.is_empty() {
            return Ok(read);
        }
        let (done, used) = match available.iter().position(|&b| b == b'\n') {
            Some(at) => (true, at + 1),
            None => (false, available.len()),
        };
        if read + used > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a line too long"));
        }
        line.extend_from_slice(&available[..used]);
        reader.consume(used);
        read += used;
        if done {
            return Ok(read);
        }
    }
}

/// The status line and the headers (names in lower case) of an answer.
fn read_head(reader: &mut impl BufRead) -> io::Result<(u16, Vec<(String, String)>)> {
    let mut size = 0;
    let mut lines = Vec::new();
    loop {
        let mut line = Vec::new();
        let read = read_line_within(reader, &mut line, MAX_HEAD - size)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "answer's head too long"))?;
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed before an answer"));
        }
        size += read;
        let line = String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string();
        if line.is_empty() {
            // A `100 Continue` goes before the answer itself.
            if lines.first().is_some_and(|status: &String| status.contains(" 100 ")) {
                lines.clear();
                continue;
            }
            break;
        }
        lines.push(line);
    }
    let mut lines = lines.into_iter();
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("not HTTP: {status_line}")))?;
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    Ok((status, headers))
}

/// The body that follows `headers`, however it is sent: in chunks, of a
/// length given, or to the end of the connection.
fn body_of<R: BufRead + Send + 'static>(reader: R, headers: &[(String, String)]) -> Box<dyn Read + Send> {
    let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
    if header("transfer-encoding").is_some_and(|coding| coding.to_ascii_lowercase().contains("chunked")) {
        return Box::new(Chunked {
            reader,
            left: 0,
            done: false,
        });
    }
    match header("content-length").and_then(|length| length.parse::<u64>().ok()) {
        Some(length) => Box::new(reader.take(length)),
        None => Box::new(reader),
    }
}

/// A body sent in chunks, read as one.
struct Chunked<R> {
    reader: R,
    /// What is left of the chunk being read.
    left: u64,
    done: bool,
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let mut line = Vec::new();
            if read_line_within(&mut self.reader, &mut line, MAX_CHUNK_LINE)? == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "a chunk cut short"));
            }
            let line = String::from_utf8_lossy(&line);
            // Its size, in hexadecimal, before any extension.
            let size = line.trim().split(';').next().unwrap_or_default();
            let size = u64::from_str_radix(size.trim(), 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("not a chunk's size: {}", line.trim())))?;
            if size == 0 {
                // The trailers, to the empty line that ends them.
                let mut left = MAX_HEAD;
                loop {
                    let mut trailer = Vec::new();
                    let read = read_line_within(&mut self.reader, &mut trailer, left)?;
                    if read == 0 || trailer.iter().all(u8::is_ascii_whitespace) {
                        break;
                    }
                    left -= read;
                }
                self.done = true;
                return Ok(0);
            }
            self.left = size;
        }
        let want = buf.len().min(self.left.min(usize::MAX as u64) as usize);
        let read = self.reader.read(&mut buf[..want])?;
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "a chunk cut short"));
        }
        self.left -= read as u64;
        if self.left == 0 {
            // The line end after the chunk's data.
            read_line_within(&mut self.reader, &mut Vec::new(), MAX_CHUNK_LINE)?;
        }
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn urls_are_read_as_authorities() {
        let parse = |url: &str| Endpoint::parse(url, None).map(|e| e.authority);
        let base = |url: &str| Endpoint::parse(url, None).map(|e| e.base);
        assert_eq!(base("http://localhost:8080/opencode/?x=1").as_deref(), Some("/opencode"));
        assert_eq!(base("http://localhost:8080").as_deref(), Some(""));
        assert_eq!(base("http://localhost:8080/"), Some(String::new()));
        assert_eq!(parse("http://localhost:8080/a b"), None);
        assert_eq!(parse("http://127.0.0.1:49374").as_deref(), Some("127.0.0.1:49374"));
        assert_eq!(parse("http://127.0.0.1:4096/").as_deref(), Some("127.0.0.1:4096"));
        assert_eq!(parse("http://[::1]:4096/x?y").as_deref(), Some("[::1]:4096"));
        assert_eq!(parse("http://localhost").as_deref(), Some("localhost:80"));
        assert_eq!(parse("http://[::1]").as_deref(), Some("[::1]:80"));
        assert_eq!(parse("https://example.com"), None);
        assert_eq!(parse("http://user:pw@host:1"), None);
        assert_eq!(parse("http://"), None);
        assert_eq!(Endpoint::basic("opencode", "pw"), "Basic b3BlbmNvZGU6cHc=");
    }

    /// A server that answers one connection with `answer`, and gives back
    /// what it was asked.
    fn serve(answer: &'static [u8]) -> (Endpoint, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = Endpoint {
            authority: listener.local_addr().unwrap().to_string(),
            base: String::new(),
            auth: Some(Endpoint::basic("opencode", "secret")),
        };
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut asked = Vec::new();
            let mut buf = [0u8; 4096];
            // Up to the end of the head, and the body its length gives.
            loop {
                let n = stream.read(&mut buf).unwrap();
                asked.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&asked).to_string();
                if let Some(at) = text.find("\r\n\r\n") {
                    let length = text
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .and_then(|l| l.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if asked.len() >= at + 4 + length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            stream.write_all(answer).unwrap();
            String::from_utf8_lossy(&asked).into_owned()
        });
        (endpoint, server)
    }

    #[test]
    fn a_request_and_its_answer_whole() {
        let (endpoint, server) = serve(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"a\":true}\nNOT");
        let answer = request(&endpoint, "POST", "/api/x", Some(b"{\"b\":1}"), Duration::from_secs(5)).unwrap();
        assert!(answer.ok());
        assert_eq!(answer.text(), "{\"a\":true}\n");
        let asked = server.join().unwrap();
        assert!(asked.starts_with("POST /api/x HTTP/1.1\r\n"), "{asked}");
        assert!(asked.contains("Authorization: Basic b3BlbmNvZGU6c2VjcmV0\r\n"));
        assert!(asked.contains("Content-Length: 7\r\n"));
        assert!(asked.ends_with("\r\n\r\n{\"b\":1}"));
    }

    #[test]
    fn chunks_are_read_as_one_body() {
        let (endpoint, server) =
            serve(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;x=1\r\nhello\r\n7\r\n, world\r\n0\r\nX-Trailer: y\r\n\r\n");
        let answer = request(&endpoint, "GET", "/", None, Duration::from_secs(5)).unwrap();
        assert_eq!(answer.text(), "hello, world");
        server.join().unwrap();
    }

    #[test]
    fn an_answer_without_a_length_runs_to_the_end() {
        let (endpoint, server) = serve(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 404 Not Found\r\n\r\nnone here");
        let answer = request(&endpoint, "GET", "/", None, Duration::from_secs(5)).unwrap();
        assert_eq!(answer.status, 404);
        assert!(!answer.ok());
        assert_eq!(answer.text(), "none here");
        server.join().unwrap();
    }

    #[test]
    fn a_service_behind_a_path_is_asked_there() {
        let endpoint = Endpoint::parse("http://127.0.0.1:8080/opencode/", None).unwrap();
        let head = String::from_utf8(endpoint.head("GET", "/api/info", "application/json", None)).unwrap();
        assert!(head.starts_with("GET /opencode/api/info HTTP/1.1\r\n"), "{head}");
    }

    #[test]
    fn only_this_machine_is_asked() {
        let endpoint = Endpoint::parse("http://192.0.2.1:4096", Some(Endpoint::basic("opencode", "pw"))).unwrap();
        let err = request(&endpoint, "GET", "/api/info", None, Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    }

    #[test]
    fn a_line_without_an_end_is_cut_off() {
        let mut long = io::Cursor::new(vec![b'x'; 100]);
        let mut line = Vec::new();
        assert!(read_line_within(&mut long, &mut line, 10).is_err());
        let mut short = io::Cursor::new(b"ab\ncd".to_vec());
        let mut line = Vec::new();
        assert_eq!(read_line_within(&mut short, &mut line, 10).unwrap(), 3);
        assert_eq!(line, b"ab\n");
        line.clear();
        assert_eq!(read_line_within(&mut short, &mut line, 10).unwrap(), 2);
        assert_eq!(read_line_within(&mut short, &mut line, 10).unwrap(), 0);
        // A chunk's size that never ends.
        let mut chunked = Chunked {
            reader: io::Cursor::new(vec![b'f'; MAX_CHUNK_LINE * 2]),
            left: 0,
            done: false,
        };
        assert!(chunked.read(&mut [0u8; 16]).is_err());
        // A head that never ends.
        let mut head = io::Cursor::new([b"HTTP/1.1 200 OK\r\nX: ".to_vec(), vec![b'y'; MAX_HEAD]].concat());
        assert!(read_head(&mut head).is_err());
    }

    #[test]
    fn a_stream_is_read_line_by_line() {
        let (endpoint, server) = serve(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n\
              f\r\ndata: {\"a\":1}\n\n\r\n4\r\n: hb\r\n2\r\n\n\n\r\n0\r\n\r\n",
        );
        let (status, mut body) = stream(&endpoint, "/api/event", Duration::from_secs(5), Duration::from_secs(5)).unwrap();
        assert_eq!(status, 200);
        let mut lines = Vec::new();
        let mut line = String::new();
        while body.read_line(&mut line).unwrap() > 0 {
            lines.push(std::mem::take(&mut line));
        }
        assert_eq!(lines, ["data: {\"a\":1}\n", "\n", ": hb\n", "\n"]);
        let asked = server.join().unwrap();
        assert!(asked.starts_with("GET /api/event HTTP/1.1\r\n") && asked.contains("Accept: text/event-stream\r\n"));
    }
}
