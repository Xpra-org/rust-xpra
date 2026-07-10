use std::io::{self, Read, Write};
use std::net::TcpStream;

use super::websocket::WebSocketStream;

pub enum Connection {
    Tcp(TcpStream),
    WebSocket(WebSocketStream),
}

impl Connection {
    pub fn try_clone(&self) -> io::Result<Connection> {
        match self {
            Connection::Tcp(stream) => Ok(Connection::Tcp(stream.try_clone()?)),
            Connection::WebSocket(stream) => Ok(Connection::WebSocket(stream.try_clone()?)),
        }
    }
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Connection::Tcp(stream) => stream.read(buf),
            Connection::WebSocket(stream) => stream.read(buf),
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Connection::Tcp(stream) => stream.write(buf),
            Connection::WebSocket(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Connection::Tcp(stream) => stream.flush(),
            Connection::WebSocket(stream) => stream.flush(),
        }
    }
}
