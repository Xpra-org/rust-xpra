// Minimal RFC 6455 WebSocket client: just enough to perform the opening
// handshake and exchange binary frames carrying the same xpra packet stream
// that a plain `tcp://` connection would carry. Deliberately hand-rolled
// (instead of pulling in a websocket crate) since the actual protocol needed
// here is small: an HTTP upgrade request/response, a SHA1+base64 accept
// check, and frame masking. Generic over the underlying stream so it can wrap
// either a plain `TcpStream` (`ws://`) or a TLS session (`wss://`).
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose;
use log::trace;

use super::sha1::sha1;
use super::tls::SharedTlsStream;

const ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

const OPCODE_CONTINUATION: u8 = 0x0;
const OPCODE_TEXT: u8 = 0x1;
const OPCODE_BINARY: u8 = 0x2;
const OPCODE_CLOSE: u8 = 0x8;
const OPCODE_PING: u8 = 0x9;
const OPCODE_PONG: u8 = 0xA;

// bridges std's fallible `TcpStream::try_clone` and `SharedTlsStream`'s cheap
// `Arc` clone under one interface, so `WebSocketStream<S>` can be cloned for
// the reader thread regardless of which transport it wraps. `write_frame` is
// its own method (rather than relying on `Write::write_all`) so that a TLS
// connection can hold its lock for one whole frame instead of once per
// underlying short write, which would let a concurrent writer (the reader
// thread's automatic pong reply to a ping) interleave into the middle of it.
pub trait CloneableStream: Read + Write + Sized {
    fn try_clone(&self) -> io::Result<Self>;
    fn write_frame(&mut self, data: &[u8]) -> io::Result<()>;
}

impl CloneableStream for TcpStream {
    fn try_clone(&self) -> io::Result<Self> {
        TcpStream::try_clone(self)
    }

    fn write_frame(&mut self, data: &[u8]) -> io::Result<()> {
        self.write_all(data)
    }
}

impl CloneableStream for SharedTlsStream {
    fn try_clone(&self) -> io::Result<Self> {
        SharedTlsStream::try_clone(self)
    }

    fn write_frame(&mut self, data: &[u8]) -> io::Result<()> {
        SharedTlsStream::write_all(self, data)
    }
}

pub struct WebSocketStream<S: CloneableStream> {
    stream: S,
    read_buf: Vec<u8>,
    read_pos: usize,
}

impl<S: CloneableStream> WebSocketStream<S> {
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
        self.stream.write_frame(&frame)
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

impl<S: CloneableStream> Read for WebSocketStream<S> {
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

impl<S: CloneableStream> Write for WebSocketStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.send_frame(OPCODE_BINARY, buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

// performs the client-side HTTP upgrade handshake on an already-connected stream.
pub fn connect<S: CloneableStream>(mut stream: S, host: &str, path: &str) -> Result<WebSocketStream<S>, String> {
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
    trace!("websocket handshake response: {:?}", response_str);
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


#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    // An in-memory peer: `input` is what the server sends us, `output` collects what we send it.
    // With `handshake` set it answers the upgrade request instead, which it can only do once it
    // has seen the nonce we generated - hence a fake that reacts rather than a canned script.
    struct Fake {
        input: Vec<u8>,
        pos: usize,
        output: Rc<RefCell<Vec<u8>>>,
        handshake: bool,
    }

    impl Fake {
        fn new(input: Vec<u8>) -> Self {
            Fake { input, pos: 0, output: Rc::new(RefCell::new(Vec::new())), handshake: false }
        }

        fn answering_the_handshake() -> Self {
            Fake { input: Vec::new(), pos: 0, output: Rc::new(RefCell::new(Vec::new())),
                   handshake: true }
        }
    }

    impl Read for Fake {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.handshake && self.pos >= self.input.len() {
                self.handshake = false;
                let request = String::from_utf8(self.output.borrow().clone()).unwrap();
                let key = request.lines()
                    .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
                    .expect("no Sec-WebSocket-Key in the request")
                    .trim()
                    .to_string();
                self.input = format!("HTTP/1.1 101 Switching Protocols\r\n\
                                      Upgrade: websocket\r\n\
                                      Sec-WebSocket-Accept: {}\r\n\r\n", accept_key(&key))
                    .into_bytes();
                self.pos = 0;
            }
            let mut source = &self.input[self.pos..];
            let read = source.read(buf)?;
            self.pos += read;
            Ok(read)
        }
    }

    impl Write for Fake {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    impl CloneableStream for Fake {
        fn try_clone(&self) -> io::Result<Self> {
            Ok(Fake { input: Vec::new(), pos: 0, output: self.output.clone(), handshake: false })
        }
        fn write_frame(&mut self, data: &[u8]) -> io::Result<()> {
            self.write_all(data)
        }
    }

    // a server->client frame, which is never masked
    fn frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![if fin { 0x80 | opcode } else { opcode }];
        if payload.len() <= 125 {
            out.push(payload.len() as u8);
        } else {
            out.push(126);
            out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        out.extend_from_slice(payload);
        out
    }

    fn socket(input: Vec<u8>) -> WebSocketStream<Fake> {
        WebSocketStream { stream: Fake::new(input), read_buf: Vec::new(), read_pos: 0 }
    }

    fn read_all(socket: &mut WebSocketStream<Fake>, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        let read = socket.read(&mut out).unwrap();
        out.truncate(read);
        out
    }

    #[test]
    fn the_accept_key_matches_the_rfc_6455_example() {
        // section 1.3 of the RFC: this exact pair is what every server computes, so getting it
        // wrong means every handshake is rejected
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn a_binary_frame_reads_back_as_its_payload() {
        let mut socket = socket(frame(true, OPCODE_BINARY, b"packet"));
        assert_eq!(read_all(&mut socket, 64), b"packet");
    }

    #[test]
    fn a_fragmented_message_is_reassembled_before_it_is_served() {
        // the xpra packet layer above reads whole packets, so a message split across frames must
        // not surface as two short reads
        let mut wire = frame(false, OPCODE_BINARY, b"one ");
        wire.extend(frame(false, OPCODE_CONTINUATION, b"two "));
        wire.extend(frame(true, OPCODE_CONTINUATION, b"three"));
        let mut socket = socket(wire);
        assert_eq!(read_all(&mut socket, 64), b"one two three");
    }

    #[test]
    fn a_ping_is_answered_with_a_pong_and_does_not_interrupt_the_data() {
        let mut wire = frame(true, OPCODE_PING, b"hi");
        wire.extend(frame(true, OPCODE_BINARY, b"packet"));
        let mut socket = socket(wire);
        assert_eq!(read_all(&mut socket, 64), b"packet");
        // the pong went back masked (client->server frames always are), so only the header and
        // the length are checked here
        let sent = socket.stream.output.borrow().clone();
        assert_eq!(sent[0], 0x80 | OPCODE_PONG);
        assert_eq!(sent[1], 0x80 | 2);
        assert_eq!(sent.len(), 2 + 4 + 2);
    }

    #[test]
    fn an_unsolicited_pong_is_skipped() {
        let mut wire = frame(true, OPCODE_PONG, b"late");
        wire.extend(frame(true, OPCODE_BINARY, b"packet"));
        let mut socket = socket(wire);
        assert_eq!(read_all(&mut socket, 64), b"packet");
    }

    #[test]
    fn a_close_frame_ends_the_stream() {
        let mut socket = socket(frame(true, OPCODE_CLOSE, b""));
        assert_eq!(socket.read(&mut [0u8; 8]).unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
    }

    #[test]
    fn an_unknown_opcode_is_rejected() {
        let mut socket = socket(frame(true, 0x0B, b""));
        assert_eq!(socket.read(&mut [0u8; 8]).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_payload_over_125_bytes_uses_the_extended_length() {
        // the length field switches to a 16-bit extension at 126, the boundary worth pinning
        let payload = vec![b'x'; 300];
        let mut socket = socket(frame(true, OPCODE_BINARY, &payload));
        assert_eq!(read_all(&mut socket, 512), payload);
    }

    #[test]
    fn the_handshake_asks_for_the_binary_subprotocol() {
        let fake = Fake::answering_the_handshake();
        let output = fake.output.clone();
        connect(fake, "server:10000", "/ws").unwrap();
        let request = String::from_utf8(output.borrow().clone()).unwrap();
        // without this header the xpra server refuses the upgrade
        assert!(request.contains("Sec-WebSocket-Protocol: binary\r\n"), "{request}");
        assert!(request.starts_with("GET /ws HTTP/1.1\r\n"), "{request}");
        assert!(request.contains("Host: server:10000\r\n"), "{request}");
        assert!(request.contains("Sec-WebSocket-Version: 13\r\n"), "{request}");
    }

    #[test]
    fn an_empty_path_becomes_a_slash() {
        let fake = Fake::answering_the_handshake();
        let output = fake.output.clone();
        connect(fake, "server:10000", "").unwrap();
        assert!(String::from_utf8(output.borrow().clone()).unwrap().starts_with("GET / HTTP/1.1\r\n"));
    }

    #[test]
    fn a_handshake_that_is_not_a_101_is_rejected() {
        // a plain http server on the port, or an xpra server without websocket support
        let response = b"HTTP/1.1 404 Not Found\r\n\r\n".to_vec();
        assert!(connect(Fake::new(response), "server:10000", "/").is_err());
    }

    #[test]
    fn a_wrong_accept_header_is_rejected() {
        // the accept hash is what proves the peer really spoke websocket rather than echoing
        let response = b"HTTP/1.1 101 Switching Protocols\r\n\
                         Sec-WebSocket-Accept: bm90IHRoZSByaWdodCBoYXNo\r\n\r\n".to_vec();
        assert!(connect(Fake::new(response), "server:10000", "/").is_err());
        // as is one that is missing altogether
        let response = b"HTTP/1.1 101 Switching Protocols\r\n\r\n".to_vec();
        assert!(connect(Fake::new(response), "server:10000", "/").is_err());
    }
}
