// Minimal RFC 6455 WebSocket client: just enough to perform the opening
// handshake and exchange binary frames carrying the same xpra packet stream
// that a plain `tcp://` connection would carry. Deliberately hand-rolled
// (instead of pulling in a websocket crate) since the actual protocol needed
// here is small: an HTTP upgrade request/response, a SHA1+base64 accept
// check, and frame masking.
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose;
use log::trace;

use super::sha1::sha1;

const ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

const OPCODE_CONTINUATION: u8 = 0x0;
const OPCODE_TEXT: u8 = 0x1;
const OPCODE_BINARY: u8 = 0x2;
const OPCODE_CLOSE: u8 = 0x8;
const OPCODE_PING: u8 = 0x9;
const OPCODE_PONG: u8 = 0xA;

pub struct WebSocketStream {
    stream: TcpStream,
    read_buf: Vec<u8>,
    read_pos: usize,
}

impl WebSocketStream {
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(WebSocketStream {
            stream: self.stream.try_clone()?,
            read_buf: Vec::new(),
            read_pos: 0,
        })
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode); // FIN=1
        let len = payload.len();
        if len <= 125 {
            frame.push(0x80 | len as u8);
        } else if len <= 0xFFFF {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        let mask_key = random_bytes4();
        frame.extend_from_slice(&mask_key);
        let start = frame.len();
        frame.extend_from_slice(payload);
        for (i, b) in frame[start..].iter_mut().enumerate() {
            *b ^= mask_key[i % 4];
        }
        self.stream.write_all(&frame)?;
        self.stream.flush()
    }

    // reads one full (possibly reassembled from fragments) data message into `read_buf`,
    // transparently answering pings and skipping unsolicited pongs.
    fn fill_buffer(&mut self) -> io::Result<()> {
        let mut message = Vec::new();
        loop {
            let mut header = [0u8; 2];
            self.stream.read_exact(&mut header)?;
            let fin = header[0] & 0x80 != 0;
            let opcode = header[0] & 0x0F;
            let masked = header[1] & 0x80 != 0;
            let mut len = (header[1] & 0x7F) as u64;
            if len == 126 {
                let mut ext = [0u8; 2];
                self.stream.read_exact(&mut ext)?;
                len = u16::from_be_bytes(ext) as u64;
            } else if len == 127 {
                let mut ext = [0u8; 8];
                self.stream.read_exact(&mut ext)?;
                len = u64::from_be_bytes(ext);
            }
            let mask_key = if masked {
                let mut mk = [0u8; 4];
                self.stream.read_exact(&mut mk)?;
                Some(mk)
            } else {
                None
            };
            let mut payload = vec![0u8; len as usize];
            self.stream.read_exact(&mut payload)?;
            if let Some(mk) = mask_key {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mk[i % 4];
                }
            }
            trace!("websocket frame opcode={:#x} fin={:?} len={:?}", opcode, fin, len);
            match opcode {
                OPCODE_PING => {
                    self.send_frame(OPCODE_PONG, &payload)?;
                }
                OPCODE_PONG => {}
                OPCODE_CLOSE => {
                    return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "websocket closed by peer"));
                }
                OPCODE_CONTINUATION | OPCODE_TEXT | OPCODE_BINARY => {
                    message.extend_from_slice(&payload);
                    if fin {
                        self.read_buf = message;
                        self.read_pos = 0;
                        return Ok(());
                    }
                }
                other => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unsupported websocket opcode {other:#x}")));
                }
            }
        }
    }
}

impl Read for WebSocketStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.read_pos >= self.read_buf.len() {
            self.fill_buffer()?;
        }
        let available = &self.read_buf[self.read_pos..];
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.read_pos += n;
        Ok(n)
    }
}

impl Write for WebSocketStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.send_frame(OPCODE_BINARY, buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

// performs the client-side HTTP upgrade handshake on an already-connected TCP stream.
pub fn connect(mut stream: TcpStream, host: &str, path: &str) -> Result<WebSocketStream, String> {
    let request_path = if path.is_empty() { "/" } else { path };
    let key = general_purpose::STANDARD.encode(random_bytes16());
    let request = format!(
        "GET {request_path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Protocol: binary\r\n\
         \r\n"
    );
    stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;

    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).map_err(|e| e.to_string())?;
        response.push(byte[0]);
        if response.ends_with(b"\r\n\r\n") {
            break;
        }
        if response.len() > 8192 {
            return Err("websocket handshake response too large".to_string());
        }
    }
    let response_str = String::from_utf8_lossy(&response);
    let mut lines = response_str.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    if !status_line.contains(" 101 ") {
        return Err(format!("unexpected websocket handshake response: {status_line:?}"));
    }
    let mut accept_header = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("sec-websocket-accept") {
                accept_header = Some(v.trim().to_string());
            }
        }
    }
    let expected = accept_key(&key);
    match &accept_header {
        Some(accept) if *accept == expected => {}
        other => return Err(format!("invalid Sec-WebSocket-Accept: got {other:?}, expected {expected:?}")),
    }
    Ok(WebSocketStream { stream, read_buf: Vec::new(), read_pos: 0 })
}

fn accept_key(client_key: &str) -> String {
    let combined = format!("{client_key}{ACCEPT_GUID}");
    general_purpose::STANDARD.encode(sha1(combined.as_bytes()))
}

// non-cryptographic randomness: fine for a WebSocket handshake nonce and
// frame masking key, neither of which is a security boundary (masking only
// exists to defeat cache-poisoning of naive intermediary proxies).
static SEED_COUNTER: AtomicU64 = AtomicU64::new(0);

fn xorshift_next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn random_bytes4() -> [u8; 4] {
    let bytes = random_bytes8();
    [bytes[0], bytes[1], bytes[2], bytes[3]]
}

fn random_bytes16() -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..8].copy_from_slice(&random_bytes8());
    out[8..16].copy_from_slice(&random_bytes8());
    out
}

fn random_bytes8() -> [u8; 8] {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
    let count = SEED_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut state = (nanos ^ count.wrapping_mul(0x9E3779B97F4A7C15)) | 1;
    xorshift_next(&mut state).to_le_bytes()
}
