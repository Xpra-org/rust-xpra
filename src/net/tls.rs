// TLS transport (`ssl://` / `wss://`), backed by `native-tls`: OpenSSL on Linux,
// Schannel on Windows, Security.framework on macOS. Unlike WebSocket masking,
// TLS encryption is a real security boundary, so this is a thin wrapper around
// an audited library rather than a hand-rolled implementation.
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use native_tls::{TlsConnector, TlsStream};

// A single TLS session object is not safe for concurrent use by two threads
// (see xpra's own `SSLSocketConnection`, which hits the same issue with
// OpenSSL); this client's reader thread and UI thread (writer) share one
// `TlsStream`, guarded by a mutex.
//
// The socket is put in true non-blocking mode (not a read *timeout* on an
// otherwise-blocking socket): OpenSSL only guarantees it's safe to retry a
// call after `WouldBlock` when the underlying transport is genuinely
// non-blocking (`SSL_ERROR_WANT_READ`/`WANT_WRITE`). A timeout expiring on a
// nominally-blocking socket is treated as a plain I/O error instead, which
// left the TLS record parser in a corrupted, misaligned state after any read
// that happened to time out mid-record - this was reproducible by moving the
// mouse/typing (triggering writes) while data was arriving.
const RETRY_INTERVAL: Duration = Duration::from_millis(5);

pub fn connect(stream: TcpStream, hostname: &str) -> Result<SharedTlsStream, String> {
    let connector = TlsConnector::builder()
        // xpra servers commonly use self-signed certificates for local/test
        // setups, and this client has no way to configure a custom CA yet;
        // accept whatever certificate is presented rather than hard failing.
        // This connection is not authenticated against a trusted CA.
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .map_err(|e| e.to_string())?;
    // the handshake itself runs on the still-blocking socket, before any
    // second thread exists to contend for it.
    let tls_stream = connector.connect(hostname, stream).map_err(|e| e.to_string())?;
    SharedTlsStream::new(tls_stream).map_err(|e| e.to_string())
}

#[derive(Clone)]
pub struct SharedTlsStream {
    inner: Arc<Mutex<TlsStream<TcpStream>>>,
}

impl SharedTlsStream {
    fn new(stream: TlsStream<TcpStream>) -> io::Result<Self> {
        stream.get_ref().set_nonblocking(true)?;
        Ok(SharedTlsStream { inner: Arc::new(Mutex::new(stream)) })
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(SharedTlsStream { inner: self.inner.clone() })
    }

    // writes a whole buffer while holding the lock for the entire operation,
    // including across `WouldBlock` retries (as opposed to `Write::write_all`'s
    // default impl, which would call our `write()` once per underlying short
    // write, each independently locking): a second writer (the reader thread's
    // automatic pong reply to a ping) must never be able to interleave its
    // bytes into the middle of this one. A write only ever blocks briefly
    // (waiting for local socket buffer space), unlike a read, so this can't
    // meaningfully starve a concurrent reader.
    pub fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        let mut guard = self.inner.lock().unwrap();
        let mut offset = 0;
        while offset < buf.len() {
            match guard.write(&buf[offset..]) {
                Ok(n) => offset += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(RETRY_INTERVAL),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl Read for SharedTlsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.inner.lock().unwrap();
            match guard.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    drop(guard);
                    thread::sleep(RETRY_INTERVAL);
                    continue;
                }
                other => return other,
            }
        }
    }
}

impl Write for SharedTlsStream {
    // only used via the generic `Write` trait (e.g. by the handshake code,
    // which runs before any second writer thread exists); application traffic
    // goes through the dedicated `write_all` above instead.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.inner.lock().unwrap();
            match guard.write(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    drop(guard);
                    thread::sleep(RETRY_INTERVAL);
                    continue;
                }
                other => return other,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.lock().unwrap().flush()
    }
}
