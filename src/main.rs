extern crate alloc;

use std::env;
use std::net::TcpStream;
use std::path::Path;
use std::process;
use std::sync::Arc;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use log::{debug, error, info, LevelFilter};
use softbuffer::Context;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy, OwnedDisplayHandle};
use winit::window::WindowId;
use xpra::CLIENT_VERSION;
use xpra::exit_codes::ExitCode;
use xpra::net::connection::Connection;
use xpra::net::packet::Packet;
use xpra::net::uri::{host_only, parse_target, Scheme, Target};
use xpra::net::{ssh, tls, websocket};

mod client;
use client::client::{client_packet, XpraClient};
use client::connect_dialog::{ConnectAction, ConnectDetails, ConnectDialog};
use client::mmap::MmapArea;
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


// The body of `--help`, minus the usage line, which needs the program name. Kept in step with the
// man page (packaging/rust-xpra.1), which is the long form of the same thing.
const HELP: &str = "\
Xpra client: connects to an xpra server and shows the windows of the applications
running there on the local desktop.

With no TARGET at all, a dialog asks for the connection details.

Targets:
  HOST:PORT                           plain tcp, the same as tcp://HOST:PORT/
  tcp://HOST:PORT/                    plain tcp
  ssl://HOST:PORT/                    tcp with TLS
  ws://HOST:PORT/                     websocket over http
  wss://HOST:PORT/                    websocket over https
  ssh://[USER@]HOST[:PORT]/[DISPLAY]  tunnel through the system 'ssh' (port 22 by default)

ssl:// and wss:// verify the server's certificate chain and hostname against the
system trust store. There is no way to trust a private CA yet, so a self-signed
certificate needs --ssl-insecure.

Options:
  -h, --help                          show this help and exit
      --version                       show the version and exit
      --ssl-insecure                  connect to an ssl:// or wss:// server without
                                      verifying its certificate or hostname

Environment:
  XPRA_PASSWORD     the session password, used to answer the server's authentication
                    challenge without prompting
  PINENTRY_PROGRAM  the pinentry binary to prompt with, when there is no XPRA_PASSWORD
  XPRA_MMAP         'no' to switch off shared memory picture transfers, or the path of
                    the backing file to use for them (Linux only, on by default)
  XPRA_MMAP_DIR     the directory to create the shared memory file in (the temporary
                    directory by default)
  XPRA_MMAP_SIZE    the size of the shared memory area, with an optional K/M/G suffix
                    (128M by default, 64M minimum)
  NO_COLOR          never colour the log output

See rust-xpra(1), or https://github.com/Xpra-org/rust-xpra, for the full documentation.
";

// The name to show in the usage message: the distribution packages install the binary as
// `rust-xpra` (see packaging/README.md) while cargo builds an `xpra`, so take it from argv[0].
fn program_name(args: &[String]) -> &str {
    args.first()
        .and_then(|arg0| Path::new(arg0).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("xpra")
}

// Everything the command line can say, once `--help` and `--version` have had their turn.
#[derive(Default)]
struct Options {
    // the target as the user typed it; `None` means the connection dialog collects one instead.
    target: Option<String>,
    // `--ssl-insecure`: connect to an `ssl://`/`wss://` server without verifying its certificate
    // chain or hostname. Off by default - see net::tls.
    ssl_insecure: bool,
}

// Options and the target may come in either order, and there is at most one target. Unlike the
// rest of the command line this is strict about spelling: an unrecognized `-...` argument is an
// error rather than something to connect to, so a mistyped option can never be read as a hostname.
fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    for arg in args.iter().skip(1) {
        match arg.as_str() {
            // dealt with before this runs, but they are still valid arguments:
            "-h" | "--help" | "--version" => {}
            "--ssl-insecure" => options.ssl_insecure = true,
            _ if arg.starts_with('-') => return Err(format!("unrecognized option {:?}", arg)),
            _ => match &options.target {
                Some(first) => return Err(format!("more than one target: {:?} and {:?}", first, arg)),
                None => options.target = Some(arg.clone()),
            },
        }
    }
    Ok(options)
}

fn run(log_sink: LogSink) -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let program = program_name(&args);
    // asking for help wins over anything else on the command line, however invalid the rest is.
    // Printed rather than logged: it is the output the user asked for, not a log record.
    if args.iter().skip(1).any(|arg| arg == "-h" || arg == "--help") {
        println!("usage: {program} [OPTIONS] [TARGET]");
        print!("\n{HELP}");
        return ExitCode::Ok;
    }
    // this client's own version, and only that: the protocol version announced to the server
    // (`xpra::VERSION`) is a different thing entirely and has no business here.
    if args.iter().skip(1).any(|arg| arg == "--version") {
        println!("{program} {CLIENT_VERSION}");
        return ExitCode::Ok;
    }
    let options = match parse_args(&args) {
        Ok(options) => options,
        Err(message) => {
            error!("{}", message);
            error!("usage: {program} [OPTIONS] [TARGET]");
            error!("try '{program} --help' for more information");
            return ExitCode::ArgumentMismatch;
        }
    };
    let ssl_insecure = options.ssl_insecure;
    // with a target on the command line we connect before doing anything else, so that a bad
    // address is reported (and exited on) without ever opening a window. With no argument, the
    // connection dialog collects one instead - see AppState below.
    let session = match &options.target {
        Some(target_str) => {
            let target = match parse_target(target_str) {
                Ok(target) => target,
                Err(message) => {
                    error!("{}", message);
                    error!("try '{program} --help' for more information");
                    return ExitCode::ArgumentMismatch;
                }
            };
            match connect(&target, ssl_insecure) {
                Ok(connection) => Some((connection, target_str.clone())),
                Err((exit_code, message)) => {
                    error!("{}", message);
                    return exit_code;
                }
            }
        }
        None => None,
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
    // the mmap area, if we can have one: offered to the server in the hello, and read from by the
    // decode thread in place of a decoder for the `mmap` encoding.
    let mmap = MmapArea::create().map(Arc::new);
    XpraClient::start_draw_decode_loop(proxy.clone(), decode_rx, mmap.clone());

    let mut app = App::new(proxy, decode_tx, log_sink, mmap, ssl_insecure);
    if let Some((connection, target)) = session {
        // args[1] as typed, rather than the parsed target: it is what the user will recognise in
        // the system tray's tooltip and menu header (see client/tray.rs).
        app.state = AppState::Session(app.new_client(connection, target, None, None));
    }
    if let Err(e) = event_loop.run_app(&mut app) {
        error!("event loop error: {}", e);
        return ExitCode::InternalError;
    }
    app.exit_code()
}


// The `ApplicationHandler` for the whole process. winit allows a single event loop per process, so
// the connection dialog cannot run its own before the client's: both are states of this one
// handler, which starts in `Prompt` when there was no target on the command line and switches to
// `Session` once a connection is up. Everything it does beyond that is delegation to XpraClient -
// which implements `ApplicationHandler` itself, and must keep having every callback it implements
// forwarded here.
struct App {
    state: AppState,
    // the softbuffer context, created for the dialog and handed over to the client with the rest of
    // its window state; `None` once the session owns it (or until `resumed` runs).
    context: Option<Context<OwnedDisplayHandle>>,
    proxy: EventLoopProxy<Packet>,
    decode_sender: Sender<Packet>,
    log_sink: LogSink,
    // the shared memory area for mmap picture transfers (client/mmap.rs), created once for the
    // process and shared with the decode thread. It depends on nothing but its own size, so it is
    // ready before we know whether the target came from the command line or from the dialog.
    mmap: Option<Arc<MmapArea>>,
    // `--ssl-insecure`, applied to whatever the dialog ends up connecting to (the flag is given
    // before the protocol is picked, so `connect` is what rejects it on a non-TLS target).
    ssl_insecure: bool,
    // the connection attempt started from the dialog: what the user asked for, and the channel the
    // worker thread hands the outcome back on (see start_connect / finish_connect).
    pending: Option<ConnectDetails>,
    connect_rx: Option<Receiver<Result<Connection, (ExitCode, String)>>>,
    // set when the dialog is cancelled or cannot be shown; the session's own exit code wins.
    exit_code: Option<ExitCode>,
}

enum AppState {
    // no target on the command line: the dialog collects one. `None` until `resumed` creates the
    // window, which needs the `ActiveEventLoop` we only get there.
    Prompt(Option<ConnectDialog>),
    Session(XpraClient),
}

// A client-side packet type (like "draw-decoded" or "send-ping"): the connect worker thread posts
// it to tell the UI thread that its result is waiting on the channel.
const CONNECT_RESULT: &str = "connect-result";

impl App {
    fn new(proxy: EventLoopProxy<Packet>, decode_sender: Sender<Packet>, log_sink: LogSink,
           mmap: Option<Arc<MmapArea>>, ssl_insecure: bool) -> Self {
        App {
            state: AppState::Prompt(None),
            context: None,
            proxy,
            decode_sender,
            log_sink,
            mmap,
            ssl_insecure,
            pending: None,
            connect_rx: None,
            exit_code: None,
        }
    }

    fn new_client(
        &self,
        connection: Connection,
        target: String,
        username: Option<String>,
        password: Option<String>,
    ) -> XpraClient {
        let mut client = XpraClient::new(
            connection,
            self.proxy.clone(),
            self.decode_sender.clone(),
            self.log_sink.clone(),
            target,
            self.mmap.clone(),
        );
        client.username = username;
        client.password = password;
        client
    }

    fn dialog(&mut self) -> Option<&mut ConnectDialog> {
        match &mut self.state {
            AppState::Prompt(dialog) => dialog.as_mut(),
            AppState::Session(_) => None,
        }
    }

    fn quit(&mut self, event_loop: &ActiveEventLoop, exit_code: ExitCode) {
        if self.exit_code.is_none() {
            self.exit_code = Some(exit_code);
        }
        event_loop.exit();
    }

    // whatever ended the session: a server `disconnect`, a lost connection, a clean exit - or, if
    // we never got that far, whatever ended the dialog.
    fn exit_code(&self) -> ExitCode {
        match &self.state {
            AppState::Session(client) => client.exit_code,
            AppState::Prompt(_) => None,
        }
        .or(self.exit_code)
        .unwrap_or(ExitCode::Ok)
    }

    fn show_dialog(&mut self, event_loop: &ActiveEventLoop) {
        let context = match Context::new(event_loop.owned_display_handle()) {
            Ok(context) => context,
            Err(e) => {
                error!("failed to create the softbuffer context: {:?}", e);
                self.quit(event_loop, ExitCode::InternalError);
                return;
            }
        };
        match ConnectDialog::new(event_loop, &context) {
            Ok(dialog) => {
                self.context = Some(context);
                self.state = AppState::Prompt(Some(dialog));
            }
            Err(e) => {
                error!("{e}");
                self.quit(event_loop, ExitCode::InternalError);
            }
        }
    }

    fn handle_dialog_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        let Some(dialog) = self.dialog() else {
            return;
        };
        if dialog.window.id() != window_id {
            return;
        }
        let action = match event {
            // the dialog re-reads its size and the display's scale factor on every draw, so a
            // resize or a move to a differently-scaled monitor only needs one:
            WindowEvent::RedrawRequested
            | WindowEvent::Resized(_)
            | WindowEvent::ScaleFactorChanged { .. } => {
                dialog.draw();
                return;
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                dialog.set_modifiers(modifiers.state());
                return;
            }
            WindowEvent::CursorMoved { position, .. } => {
                dialog.set_pointer(position);
                return;
            }
            WindowEvent::MouseInput { state, button, .. } => dialog.handle_mouse(state, button),
            // winit synthesizes a press for every key already held when a window takes focus, and
            // starting the client from a shell routinely leaves one down: typing those would put a
            // stray character in the focused field.
            WindowEvent::KeyboardInput { is_synthetic: true, .. } => return,
            WindowEvent::KeyboardInput { event: key_event, .. } => dialog.handle_key(&key_event),
            WindowEvent::CloseRequested => ConnectAction::Cancel,
            _ => return,
        };
        match action {
            ConnectAction::None => {}
            ConnectAction::Cancel => {
                info!("connection cancelled");
                self.quit(event_loop, ExitCode::Ok);
            }
            ConnectAction::Connect(details) => self.start_connect(details),
        }
    }

    // Connect on a worker thread: `connect` blocks (a dropped SYN takes tens of seconds to time
    // out, and ssh may be waiting on a host-key confirmation), and the UI thread must keep drawing
    // the dialog. The outcome comes back the way the pinentry and decode threads report theirs -
    // over a channel, with a synthesized client-side packet to wake the UI thread up.
    fn start_connect(&mut self, details: ConnectDetails) {
        let target = match parse_target(&details.uri) {
            Ok(target) => target,
            Err(message) => {
                if let Some(dialog) = self.dialog() {
                    dialog.set_error(message);
                }
                return;
            }
        };
        info!("connecting to {}", details.uri);
        if let Some(dialog) = self.dialog() {
            dialog.set_connecting(&details.uri);
        }
        let (tx, rx) = channel();
        self.connect_rx = Some(rx);
        self.pending = Some(details);
        let proxy = self.proxy.clone();
        let ssl_insecure = self.ssl_insecure;
        thread::Builder::new().name("connect".to_string()).spawn(move || {
            let _ = tx.send(connect(&target, ssl_insecure));
            let _ = proxy.send_event(client_packet(CONNECT_RESULT, ""));
        }).unwrap();
    }

    fn finish_connect(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(rx), Some(details)) = (self.connect_rx.take(), self.pending.take()) else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(connection)) => self.start_session(event_loop, connection, details),
            Ok(Err((_exit_code, message))) => {
                // unlike the command-line path this is not fatal: report it in the dialog and let
                // the user correct the details and try again.
                error!("{message}");
                if let Some(dialog) = self.dialog() {
                    dialog.set_error(message);
                }
            }
            Err(e) => error!("no connection result: {e}"),
        }
    }

    fn start_session(&mut self, event_loop: &ActiveEventLoop, connection: Connection, details: ConnectDetails) {
        let mut client = self.new_client(connection, details.uri, details.username, details.password);
        // hand over the dialog's softbuffer context rather than making a second one; dropping the
        // dialog below closes its window.
        client.softbuffer_ctx = self.context.take();
        self.state = AppState::Session(client);
        // winit only calls `resumed` once, at startup - by which time we were still showing the
        // dialog - so the client's own startup (read loop, hello, system tray) happens here.
        if let AppState::Session(client) = &mut self.state {
            client.resumed(event_loop);
        }
    }
}


impl ApplicationHandler<Packet> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        match &mut self.state {
            AppState::Session(client) => client.resumed(event_loop),
            AppState::Prompt(Some(_)) => {}
            AppState::Prompt(None) => self.show_dialog(event_loop),
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, packet: Packet) {
        if let AppState::Session(client) = &mut self.state {
            client.user_event(event_loop, packet);
            return;
        }
        // nothing generates packets before there is a session, bar the connect worker:
        if packet.len() > 0 && packet.get_str(0) == CONNECT_RESULT {
            self.finish_connect(event_loop);
        } else {
            debug!("ignoring {:?} received before the session started", packet);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        if let AppState::Session(client) = &mut self.state {
            client.window_event(event_loop, window_id, event);
            return;
        }
        self.handle_dialog_event(event_loop, window_id, event);
    }
}


// Failures here mean we never had a session at all, so they map to the "failed to connect"
// family of exit codes rather than `ConnectionLost`.
fn connect(target: &Target, ssl_insecure: bool) -> Result<Connection, (ExitCode, String)> {
    // there is nothing to skip verifying on a connection that has no certificate: say so rather
    // than let the option pass unnoticed. The dialog reports this the same way it reports a bad
    // host, since the protocol is only picked once the flag has already been given.
    if ssl_insecure && !matches!(target.scheme, Scheme::Tls | Scheme::WebSocketTls) {
        return Err((ExitCode::ArgumentMismatch,
                    "--ssl-insecure only applies to ssl:// and wss:// connections".to_string()));
    }
    let tcp_connect = || {
        TcpStream::connect(&target.address).map_err(|e| {
            (ExitCode::ConnectionFailed, format!("failed to connect to {:?}: {}", target.address, e))
        })
    };
    let tls_connect = |stream| {
        tls::connect(stream, host_only(&target.address), ssl_insecure).map_err(|e| {
            // a self-signed certificate is the likely cause on an xpra server, and there is no way
            // to trust a private CA yet, so point at the one option that gets past it.
            let hint = if ssl_insecure { "" } else { " (--ssl-insecure skips verification)" };
            (ExitCode::SslFailure, format!("tls handshake failed: {}{}", e, hint))
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


#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Options, String> {
        let argv: Vec<String> = std::iter::once("xpra")
            .chain(args.iter().copied())
            .map(String::from)
            .collect();
        parse_args(&argv)
    }

    #[test]
    fn options_and_target_come_in_either_order() {
        for args in [
            ["--ssl-insecure", "ssl://example.com:10000/"],
            ["ssl://example.com:10000/", "--ssl-insecure"],
        ] {
            let options = parse(&args).unwrap();
            assert_eq!(options.target.as_deref(), Some("ssl://example.com:10000/"));
            assert!(options.ssl_insecure);
        }
    }

    #[test]
    fn no_arguments_means_the_dialog_and_verified_certificates() {
        let options = parse(&[]).unwrap();
        assert_eq!(options.target, None);
        assert!(!options.ssl_insecure);
    }

    #[test]
    fn a_target_alone_verifies_certificates() {
        let options = parse(&["wss://example.com:10000/"]).unwrap();
        assert!(!options.ssl_insecure);
    }

    // a mistyped option must never be taken for a hostname to connect to:
    #[test]
    fn misspelled_options_are_rejected() {
        assert!(parse(&["-ssl-insecure"]).is_err());
        assert!(parse(&["--ssl_insecure"]).is_err());
        assert!(parse(&["--ssl-insecure=yes"]).is_err());
        assert!(parse(&["--insecure"]).is_err());
        assert!(parse(&["tcp://example.com:10000/", "-ssl-insecure"]).is_err());
    }

    #[test]
    fn only_one_target_is_accepted() {
        assert!(parse(&["tcp://a:10000/", "tcp://b:10000/"]).is_err());
    }

    // --help and --version act before parse_args runs, but must still parse as valid arguments:
    #[test]
    fn help_and_version_are_valid_arguments() {
        assert!(parse(&["-h"]).is_ok());
        assert!(parse(&["--help"]).is_ok());
        assert!(parse(&["--version"]).is_ok());
        assert_eq!(parse(&["--version", "tcp://a:10000/"]).unwrap().target.as_deref(),
                   Some("tcp://a:10000/"));
    }
}
