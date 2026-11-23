//! A WebSocket client over a Unix socket, which is how Codex's app-server
//! daemon listens: enough of RFC 6455 for text messages, pings and a close.
//! Blocking, with the reading and writing halves on threads of their own.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// The largest message taken in. The daemon says it sends up to 16 MiB
/// unfragmented; a thread's history can be long.
const MAX_MESSAGE: usize = 32 << 20;
/// The handshake's answer may be no longer than this.
const MAX_HEAD: usize = 16 << 10;
/// How long connecting and the handshake may take.
const HANDSHAKE: Duration = Duration::from_secs(10);

pub struct Reader {
    stream: BufReader<UnixStream>,
    /// A message whose frames are still coming, while a control frame came
    /// between them.
    partial: Option<Vec<u8>>,
}

pub struct Writer {
    stream: UnixStream,
    state: u64,
}

pub enum Message {
    Text(String),
    Ping(Vec<u8>),
    Close,
}

pub fn connect(path: &Path) -> io::Result<(Reader, Writer)> {
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(HANDSHAKE))?;
    stream.set_write_timeout(Some(HANDSHAKE))?;
    let mut writer = Writer {
        stream: stream.try_clone()?,
        state: seed(),
    };
    let mut key = [0u8; 16];
    for chunk in key.chunks_mut(8) {
        let bytes = writer.next().to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    let request = format!(
        "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
        base64(&key)
    );
    writer.stream.write_all(request.as_bytes())?;

    let mut reader = BufReader::new(stream);
    let mut head = Vec::new();
    loop {
        let before = head.len();
        reader.read_until(b'\n', &mut head)?;
        if head.len() == before {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed during the handshake"));
        }
        if head.len() > MAX_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "handshake answer too long"));
        }
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let status = head.split(|&b| b == b'\n').next().unwrap_or_default();
    if !String::from_utf8_lossy(status).contains(" 101 ") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("not a WebSocket: {}", String::from_utf8_lossy(status).trim()),
        ));
    }
    let stream = reader.get_ref();
    stream.set_read_timeout(None)?;
    writer.stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    Ok((
        Reader {
            stream: reader,
            partial: None,
        },
        writer,
    ))
}

impl Writer {
    pub fn text(&mut self, text: &str) -> io::Result<()> {
        self.frame(0x1, text.as_bytes())
    }

    pub fn pong(&mut self, payload: &[u8]) -> io::Result<()> {
        self.frame(0xA, payload)
    }

    pub fn close(&mut self) -> io::Result<()> {
        // 1000: a normal close.
        self.frame(0x8, &1000u16.to_be_bytes())
    }

    fn frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | len as u8);
        } else if len <= 0xFFFF {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        // A client masks what it sends.
        let mask = (self.next() as u32).to_le_bytes();
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stream.write_all(&frame)
    }

    /// xorshift64*: masks and the key need only be unpredictable to an
    /// intermediary, and there is none on a Unix socket.
    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

impl Reader {
    /// The next whole message.
    pub fn next(&mut self) -> io::Result<Message> {
        loop {
            let mut head = [0u8; 2];
            self.stream.read_exact(&mut head)?;
            let fin = head[0] & 0x80 != 0;
            let opcode = head[0] & 0x0F;
            let masked = head[1] & 0x80 != 0;
            let mut len = u64::from(head[1] & 0x7F);
            if len == 126 {
                let mut ext = [0u8; 2];
                self.stream.read_exact(&mut ext)?;
                len = u64::from(u16::from_be_bytes(ext));
            } else if len == 127 {
                let mut ext = [0u8; 8];
                self.stream.read_exact(&mut ext)?;
                len = u64::from_be_bytes(ext);
            }
            let held = self.partial.as_ref().map_or(0, Vec::len) as u64;
            if len + held > MAX_MESSAGE as u64 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
            }
            let mut mask = [0u8; 4];
            if masked {
                self.stream.read_exact(&mut mask)?;
            }
            let mut payload = vec![0u8; len as usize];
            self.stream.read_exact(&mut payload)?;
            if masked {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
            }
            match opcode {
                0x8 => return Ok(Message::Close),
                0x9 => return Ok(Message::Ping(payload)),
                0xA => continue,
                0x1 | 0x2 => {
                    if fin {
                        return text(payload);
                    }
                    self.partial = Some(payload);
                }
                0x0 => {
                    let Some(mut message) = self.partial.take() else {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "continuation without a start"));
                    };
                    message.extend_from_slice(&payload);
                    if fin {
                        return text(message);
                    }
                    self.partial = Some(message);
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unknown WebSocket opcode {other}"),
                    ))
                }
            }
        }
    }
}

fn text(bytes: Vec<u8>) -> io::Result<Message> {
    String::from_utf8(bytes)
        .map(Message::Text)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "message is not UTF-8"))
}

fn seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        hasher.write_u128(now.as_nanos());
    }
    hasher.finish() | 1
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixListener;

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    /// A server that answers the handshake, then sends one message in
    /// two frames with a ping between them, then reads one masked frame.
    #[test]
    fn handshake_fragments_pings_and_masking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(sock.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            sock.write_all(b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\n\r\n").unwrap();
            // "hel" (text, not final), a ping, "lo" (continuation, final).
            sock.write_all(&[0x01, 3, b'h', b'e', b'l']).unwrap();
            sock.write_all(&[0x89, 1, b'p']).unwrap();
            sock.write_all(&[0x80, 2, b'l', b'o']).unwrap();
            // A 300-byte message: the 16-bit length form.
            let long = vec![b'x'; 300];
            sock.write_all(&[0x81, 126, 1, 44]).unwrap();
            sock.write_all(&long).unwrap();
            // What the client sends: masked, and unmasked here.
            let mut head = [0u8; 2];
            reader.read_exact(&mut head).unwrap();
            assert_eq!(head[0], 0x81);
            assert!(head[1] & 0x80 != 0, "a client masks");
            let len = (head[1] & 0x7F) as usize;
            let mut mask = [0u8; 4];
            reader.read_exact(&mut mask).unwrap();
            let mut payload = vec![0u8; len];
            reader.read_exact(&mut payload).unwrap();
            payload.iter_mut().enumerate().for_each(|(i, b)| *b ^= mask[i % 4]);
            assert_eq!(payload, b"{\"id\":1}");
        });
        let (mut reader, mut writer) = connect(&path).unwrap();
        assert!(matches!(reader.next().unwrap(), Message::Ping(p) if p == b"p"));
        assert!(matches!(reader.next().unwrap(), Message::Text(t) if t == "hello"));
        assert!(matches!(reader.next().unwrap(), Message::Text(t) if t.len() == 300));
        writer.text("{\"id\":1}").unwrap();
        server.join().unwrap();
    }
}
