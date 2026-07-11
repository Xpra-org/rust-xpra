use std::io::{self, Read, Write};
use std::net::TcpStream;

use super::ssh::SshStream;
use super::tls::SharedTlsStream;
use super::websocket::WebSocketStream;

pub enum Connection {
    Tcp(TcpStream),
    Tls(SharedTlsStream),
    WebSocket(WebSocketStream<TcpStream>),
    WebSocketTls(WebSocketStream<SharedTlsStream>),
    Ssh(SshStream),
}

impl Connection {
    pub fn try_clone(&self) -> io::Result<Connection> {
        match self {
            Connection::Tcp(stream) => Ok(Connection::Tcp(stream.try_clone()?)),
            Connection::Tls(stream) => Ok(Connection::Tls(stream.try_clone()?)),
            Connection::WebSocket(stream) => Ok(Connection::WebSocket(stream.try_clone()?)),
            Connection::WebSocketTls(stream) => Ok(Connection::WebSocketTls(stream.try_clone()?)),
            Connection::Ssh(stream) => Ok(Connection::Ssh(stream.try_clone()?)),
        }
    }

    // like `Write::write_all`, but for `Tls` holds the lock for the whole write
    // instead of once per underlying short write (see `SharedTlsStream::write_all`).
    // The other variants' `write()` never reports a short write on success, so a
    // single call already covers the whole buffer.
    pub fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Connection::Tls(stream) => stream.write_all(buf),
            _ => Write::write_all(self, buf),
        }
    }
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Connection::Tcp(stream) => stream.read(buf),
            Connection::Tls(stream) => stream.read(buf),
            Connection::WebSocket(stream) => stream.read(buf),
            Connection::WebSocketTls(stream) => stream.read(buf),
            Connection::Ssh(stream) => stream.read(buf),
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Connection::Tcp(stream) => stream.write(buf),
            Connection::Tls(stream) => stream.write(buf),
            Connection::WebSocket(stream) => stream.write(buf),
            Connection::WebSocketTls(stream) => stream.write(buf),
            Connection::Ssh(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Connection::Tcp(stream) => stream.flush(),
            Connection::Tls(stream) => stream.flush(),
            Connection::WebSocket(stream) => stream.flush(),
            Connection::WebSocketTls(stream) => stream.flush(),
            Connection::Ssh(stream) => stream.flush(),
        }
    }
}
