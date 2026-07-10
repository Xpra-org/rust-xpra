extern crate alloc;

use std::env;
use std::net::TcpStream;
use std::sync::mpsc::channel;
use log::{error, LevelFilter};
use simple_logger::SimpleLogger;
use winit::event_loop::EventLoop;
use xpra::net::packet::Packet;

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
        error!("usage: {:?} HOST:IP", args[0]);
        return;
    }
    let uri = args[1].clone();
    let stream = TcpStream::connect(uri).expect("connection failed");

    let event_loop = EventLoop::<Packet>::with_user_event().build().expect("failed to create event loop");
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);

    // this channel is used for sending 'draw' packets from the UI thread to the decode thread:
    let (decode_tx, decode_rx) = channel::<Packet>();
    let proxy = event_loop.create_proxy();
    XpraClient::start_draw_decode_loop(proxy.clone(), decode_rx);

    let mut client = XpraClient::new(stream, proxy, decode_tx);
    event_loop.run_app(&mut client).expect("event loop error");
}
