extern crate alloc;

use std::env;
use std::net::TcpStream;
use std::process;
use std::sync::mpsc::channel;
use log::{error, LevelFilter};
use winit::event_loop::EventLoop;
use xpra::exit_codes::ExitCode;
use xpra::net::connection::Connection;
use xpra::net::packet::Packet;
use xpra::net::uri::{host_only, parse_target, Scheme, Target};
use xpra::net::{ssh, tls, websocket};

mod client;
use client::client::XpraClient;
use client::remote_logging::{self, LogSink};


fn main() {
    let level = if cfg!(debug_assertions) {
        LevelFilter::Debug
    }
    else {
        LevelFilter::Info
    };
    // installs the global logger; the sink stays empty until the server confirms it accepts our
    // logs, at which point info-and-above records are also forwarded (see client::remote_logging).
    let log_sink = remote_logging::init(level);

    process::exit(run(log_sink).value());
}


fn run(log_sink: LogSink) -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        error!("invalid number of arguments: {:?}", args.len());
        error!("usage: {:?} HOST:PORT | tcp://HOST:PORT/ | ssl://HOST:PORT/ | ws://HOST:PORT/ | wss://HOST:PORT/ | ssh://[USER@]HOST[:PORT]/[DISPLAY]", args[0]);
        return ExitCode::ArgumentMismatch;
    }
    let target = match parse_target(&args[1]) {
        Ok(target) => target,
        Err(message) => {
            error!("{}", message);
            return ExitCode::ArgumentMismatch;
        }
    };
    let connection = match connect(&target) {
        Ok(connection) => connection,
        Err((exit_code, message)) => {
            error!("{}", message);
            return exit_code;
        }
    };

    let event_loop = match EventLoop::<Packet>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(e) => {
            error!("failed to create the event loop: {}", e);
            return ExitCode::InternalError;
        }
    };
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);

    // this channel is used for sending 'draw' packets from the UI thread to the decode thread:
    let (decode_tx, decode_rx) = channel::<Packet>();
    let proxy = event_loop.create_proxy();
    XpraClient::start_draw_decode_loop(proxy.clone(), decode_rx);

    // args[1] as typed, rather than the parsed target: it is what the user will recognise in the
    // system tray's tooltip and menu header (see client/tray.rs).
    let mut client = XpraClient::new(connection, proxy, decode_tx, log_sink, args[1].clone());
    if let Err(e) = event_loop.run_app(&mut client) {
        error!("event loop error: {}", e);
        return ExitCode::InternalError;
    }
    // whatever ended the session: a server `disconnect`, a lost connection, or a clean exit.
    client.exit_code.unwrap_or(ExitCode::Ok)
}


// Failures here mean we never had a session at all, so they map to the "failed to connect"
// family of exit codes rather than `ConnectionLost`.
fn connect(target: &Target) -> Result<Connection, (ExitCode, String)> {
    let tcp_connect = || {
        TcpStream::connect(&target.address).map_err(|e| {
            (ExitCode::ConnectionFailed, format!("failed to connect to {:?}: {}", target.address, e))
        })
    };
    let tls_connect = |stream| {
        tls::connect(stream, host_only(&target.address)).map_err(|e| {
            (ExitCode::SslFailure, format!("tls handshake failed: {}", e))
        })
    };
    // the websocket handshake is generic over the underlying stream (tcp or tls), so it can't
    // be wrapped in a closure the way the other two are:
    let ws_error = |e| (ExitCode::ConnectionFailed, format!("websocket handshake failed: {}", e));
    match target.scheme {
        Scheme::Tcp => Ok(Connection::Tcp(tcp_connect()?)),
        Scheme::Tls => Ok(Connection::Tls(tls_connect(tcp_connect()?)?)),
        Scheme::WebSocket => {
            let ws = websocket::connect(tcp_connect()?, &target.address, &target.path).map_err(ws_error)?;
            Ok(Connection::WebSocket(ws))
        }
        Scheme::WebSocketTls => {
            let tls_stream = tls_connect(tcp_connect()?)?;
            let ws = websocket::connect(tls_stream, &target.address, &target.path).map_err(ws_error)?;
            Ok(Connection::WebSocketTls(ws))
        }
        Scheme::Ssh => {
            let ssh_stream = ssh::connect(&target.address, target.username.as_deref(), &target.path)
                .map_err(|e| (ExitCode::SshFailure, format!("ssh connection failed: {}", e)))?;
            Ok(Connection::Ssh(ssh_stream))
        }
    }
}
