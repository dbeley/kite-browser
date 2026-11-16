//! Minimal WebSocket client (RFC 6455) — just enough for the Chrome DevTools
//! Protocol: text frames, fragmentation, ping/pong, close.
//!
//! Hand-rolled on purpose: no async runtime, no TLS, no extra crates. A CDP
//! connection is a handful of frames per keystroke-free action, so a blocking
//! socket with one reader thread is both simpler and faster than tokio here.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use sha1::{Digest, Sha1};

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub struct WsWriter {
    s: TcpStream,
    mask: u32,
}

impl WsWriter {
    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut hdr = Vec::with_capacity(14);
        hdr.push(0x80 | opcode);
        let len = payload.len();
        if len < 126 {
            hdr.push(0x80 | len as u8);
        } else if len <= u16::MAX as usize {
            hdr.push(0x80 | 126);
            hdr.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            hdr.push(0x80 | 127);
            hdr.extend_from_slice(&(len as u64).to_be_bytes());
        }
        // Client frames must be masked. Masking is a protocol formality here.
        self.mask = self.mask.wrapping_mul(1664525).wrapping_add(1013904223);
        let key = self.mask.to_le_bytes();
        hdr.extend_from_slice(&key);
        let mut out = Vec::with_capacity(hdr.len() + len);
        out.extend_from_slice(&hdr);
        for (i, b) in payload.iter().enumerate() {
            out.push(b ^ key[i & 3]);
        }
        self.s.write_all(&out)
    }

    pub fn send_text(&mut self, text: &str) -> io::Result<()> {
        self.send_frame(0x1, text.as_bytes())
    }

    fn send_pong(&mut self, payload: &[u8]) -> io::Result<()> {
        self.send_frame(0xA, payload)
    }
}

pub struct WsReader {
    s: TcpStream,
    pending: Vec<u8>,
    frag: Vec<u8>,
    writer: Arc<Mutex<WsWriter>>,
}

impl WsReader {
    fn take(&mut self, n: usize) -> io::Result<Vec<u8>> {
        while self.pending.len() < n {
            let mut chunk = [0u8; 16384];
            let got = self.s.read(&mut chunk)?;
            if got == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "engine closed the debug connection",
                ));
            }
            self.pending.extend_from_slice(&chunk[..got]);
        }
        let rest = self.pending.split_off(n);
        Ok(std::mem::replace(&mut self.pending, rest))
    }

    /// Next complete text message, or Ok(None) on a clean close.
    pub fn next_text(&mut self) -> io::Result<Option<String>> {
        loop {
            let h = self.take(2)?;
            let fin = h[0] & 0x80 != 0;
            let opcode = h[0] & 0x0F;
            let masked = h[1] & 0x80 != 0;
            let len7 = (h[1] & 0x7F) as usize;
            let len = match len7 {
                126 => {
                    let b = self.take(2)?;
                    u16::from_be_bytes([b[0], b[1]]) as usize
                }
                127 => {
                    let b = self.take(8)?;
                    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as usize
                }
                n => n,
            };
            let mask = if masked { Some(self.take(4)?) } else { None };
            let mut payload = if len > 0 { self.take(len)? } else { Vec::new() };
            if let Some(m) = mask {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= m[i & 3];
                }
            }
            match opcode {
                0x1 | 0x2 | 0x0 => {
                    self.frag.extend_from_slice(&payload);
                    if fin {
                        let bytes = std::mem::take(&mut self.frag);
                        return Ok(Some(String::from_utf8_lossy(&bytes).into_owned()));
                    }
                }
                0x8 => return Ok(None),
                0x9 => {
                    if let Ok(mut w) = self.writer.lock() {
                        let _ = w.send_pong(&payload);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Connect to a ws:// URL (the only scheme CDP needs on a local socket).
pub fn connect(url: &str) -> io::Result<(WsReader, Arc<Mutex<WsWriter>>)> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "only ws:// URLs supported"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rfind(':') {
        Some(i) => (
            &authority[..i],
            authority[i + 1..].parse::<u16>().unwrap_or(80),
        ),
        None => (authority, 80),
    };
    if std::env::var("KITE_DEBUG_WS").is_ok() {
        eprintln!("[ws] dialing host={host:?} port={port} path={path}");
    }

    let mut s = TcpStream::connect((host, port))?;
    s.set_nodelay(true)?;

    let nonce: Vec<u8> = {
        let mut n = [0u8; 16];
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut n))
            .is_err()
        {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(42);
            n[..8].copy_from_slice(&t.to_le_bytes());
        }
        n.to_vec()
    };
    let key = B64.encode(&nonce);

    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    s.write_all(req.as_bytes())?;

    let mut hdr = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        if s.read(&mut byte)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "engine closed during websocket handshake",
            ));
        }
        hdr.push(byte[0]);
        if hdr.ends_with(b"\r\n\r\n") {
            break;
        }
        if hdr.len() > 16384 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized handshake",
            ));
        }
    }
    let text = String::from_utf8_lossy(&hdr).into_owned();
    let status = text.lines().next().unwrap_or("").to_string();
    if !status.contains("101") {
        let hdrs = text.replace("\r\n", " | ");
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("websocket handshake rejected: {hdrs}"),
        ));
    }
    if std::env::var("KITE_DEBUG_WS").is_ok() {
        eprintln!("[ws] handshake: {}", text.lines().next().unwrap_or(""));
    }

    let mut expect = Sha1::new();
    expect.update(key.as_bytes());
    expect.update(WS_GUID.as_bytes());
    let accept = B64.encode(expect.finalize());
    let accept_ok = text.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("sec-websocket-accept:") && l.contains(&accept.to_ascii_lowercase())
    });
    if !accept_ok {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "websocket accept key mismatch",
        ));
    }

    // Anything after the header terminator is already frame data.
    let split = hdr.len().saturating_sub(4);
    let leftover = hdr[split.min(hdr.len())..].to_vec();
    let leftover = if leftover.starts_with(b"\r\n\r\n") {
        Vec::new()
    } else {
        leftover
    };

    let writer = Arc::new(Mutex::new(WsWriter {
        s: s.try_clone()?,
        mask: 0x5eed_1234,
    }));
    let reader = WsReader {
        s,
        pending: leftover,
        frag: Vec::new(),
        writer: writer.clone(),
    };
    Ok((reader, writer))
}

/// Tiny HTTP GET for the /json/* endpoints on the debug port.
/// Chromium keeps the connection alive regardless of `Connection: close`, so
/// this reads until the advertised Content-Length is satisfied (or EOF), never
/// until the socket closes.
pub fn http_get(port: u16, path: &str) -> io::Result<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(std::time::Duration::from_millis(2500)))?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes())?;

    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];
    loop {
        let want = match header_split(&buf) {
            Some(split) => {
                let head = String::from_utf8_lossy(&buf[..split]).to_ascii_lowercase();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok());
                match len {
                    Some(n) if buf.len() >= split + 4 + n => return Ok(body_of(&buf, split)),
                    Some(n) => Some(split + 4 + n),
                    None => None,
                }
            }
            None => None,
        };
        let _ = want;
        match s.read(&mut chunk) {
            Ok(0) => return Ok(raw_body(&buf)),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                // No more data: either chunked encoding (body complete as far as
                // we can tell) or a stalled server. Whatever we have is enough.
                return Ok(raw_body(&buf));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
        if buf.len() > 8 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized http response",
            ));
        }
    }
}

fn header_split(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn raw_body(buf: &[u8]) -> String {
    match header_split(buf) {
        Some(split) => body_of(buf, split),
        None => String::from_utf8_lossy(buf).into_owned(),
    }
}

fn body_of(buf: &[u8], split: usize) -> String {
    String::from_utf8_lossy(&buf[split + 4..]).into_owned()
}
