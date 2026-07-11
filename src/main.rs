extern crate alloc;

use std::env;
use std::net::TcpStream;
use std::sync::mpsc::channel;
use log::{error, LevelFilter};
use simple_logger::SimpleLogger;
use winit::event_loop::EventLoop;
use xpra::net::connection::Connection;
use xpra::net::packet::Packet;
use xpra::net::uri::{host_only, parse_target, Scheme};
use xpra::net::{tls, websocket};

mod client;
use client::client::XpraClient;


fn main() {
    let level = if cfg!(debug_assertions) {
        LevelFilter::Debug
    }
    else {
        LevelFilter::Info
    };
    SimpleLogger::new().with_level(level).init().unwrap();

    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        error!("invalid number of arguments: {:?}", args.len());
        error!("usage: {:?} HOST:PORT | tcp://HOST:PORT/ | ssl://HOST:PORT/ | ws://HOST:PORT/ | wss://HOST:PORT/", args[0]);
        return;
    }
    let target = match parse_target(&args[1]) {
        Ok(target) => target,
        Err(message) => {
            error!("{}", message);
            return;
        }
    };
    let connection = match target.scheme {
        Scheme::Tcp => {
            let stream = TcpStream::connect(&target.address).expect("connection failed");
            Connection::Tcp(stream)
        }
        Scheme::Tls => {
            let stream = TcpStream::connect(&target.address).expect("connection failed");
            let tls_stream = tls::connect(stream, host_only(&target.address)).expect("tls handshake failed");
            Connection::Tls(tls_stream)
        }
        Scheme::WebSocket => {
            let stream = TcpStream::connect(&target.address).expect("connection failed");
            let ws = websocket::connect(stream, &target.address, &target.path).expect("websocket handshake failed");
            Connection::WebSocket(ws)
        }
        Scheme::WebSocketTls => {
            let stream = TcpStream::connect(&target.address).expect("connection failed");
            let tls_stream = tls::connect(stream, host_only(&target.address)).expect("tls handshake failed");
            let ws = websocket::connect(tls_stream, &target.address, &target.path).expect("websocket handshake failed");
            Connection::WebSocketTls(ws)
        }
    };

    let event_loop = EventLoop::<Packet>::with_user_event().build().expect("failed to create event loop");
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);

    // this channel is used for sending 'draw' packets from the UI thread to the decode thread:
    let (decode_tx, decode_rx) = channel::<Packet>();
    let proxy = event_loop.create_proxy();
    XpraClient::start_draw_decode_loop(proxy.clone(), decode_rx);

    let mut client = XpraClient::new(connection, proxy, decode_tx);
    event_loop.run_app(&mut client).expect("event loop error");
}
