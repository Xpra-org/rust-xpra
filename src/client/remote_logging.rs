use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use log::{Level, LevelFilter, Log, Metadata, Record};
use simple_logger::SimpleLogger;
use winit::event_loop::EventLoopProxy;
use yaml_rust2::Yaml;

use xpra::net::packet::Packet;

// The proxy the logger forwards records through, shared with the UI thread so it can be installed
// once the server's hello confirms it accepts client logs (XpraClient::process_hello). `None` until
// then, so nothing is forwarded before the session is up - and never at all to a server that
// doesn't want it. A Mutex (not just the Send proxy) because the logger is a Sync global.
pub type LogSink = Arc<Mutex<Option<EventLoopProxy<Packet>>>>;

// We forward records at this severity or above (Error/Warn/Info) to the server; Debug/Trace stay
// local. `log`'s ordering puts the most severe first, so "Info and above" is `level <= Info`.
const FORWARD_LEVEL: Level = Level::Info;

thread_local! {
    // Guards against a forward re-entering the logger (should `send_event`, or anything under it,
    // itself log) - mirrors xpra's `in_remote_logging` flag; without it one record could recurse.
    static IN_FORWARD: Cell<bool> = Cell::new(false);
}

// A `log::Log` that prints locally (via SimpleLogger, exactly as before) and additionally forwards
// info-and-above records to the xpra server as `logging` packets, so they land in the server log.
struct RemoteLogger {
    local: SimpleLogger,
    sink: LogSink,
}

impl Log for RemoteLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.local.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        // local output first, unchanged - so nothing is lost even if forwarding is off or fails:
        self.local.log(record);
        if record.level() > FORWARD_LEVEL {
            return;
        }
        // only forward once the server has confirmed it receives logging (sink filled in):
        let proxy = match self.sink.lock() {
            Ok(guard) => match guard.as_ref() {
                Some(proxy) => proxy.clone(),
                None => return,
            },
            Err(_) => return,
        };
        // don't let logging triggered *by* our own forward recurse (already logged locally above):
        if IN_FORWARD.with(|f| f.replace(true)) {
            return;
        }
        let packet = log_packet(record.level(), format!("{}", record.args()));
        // the UI thread turns this into the wire `logging` packet; if it is gone we're exiting.
        let _ = proxy.send_event(packet);
        IN_FORWARD.with(|f| f.set(false));
    }

    fn flush(&self) {
        self.local.flush();
    }
}

// Build the client-side "send-log" packet the UI thread turns into a wire `logging` packet
// (XpraClient::send_log). Carries the python logging level and the already-formatted message.
fn log_packet(level: Level, message: String) -> Packet {
    Packet {
        main: vec![
            Yaml::String("send-log".to_string()),
            Yaml::Integer(python_level(level)),
            Yaml::String(message),
        ],
        raw: HashMap::new(),
        decode_time_us: None,
    }
}

// Map a `log` level to the python `logging` module's numeric level, which is what the xpra logging
// packet carries and the server feeds straight into `logging.log(level, ...)`.
fn python_level(level: Level) -> i64 {
    match level {
        Level::Error => 40, // logging.ERROR
        Level::Warn => 30,  // logging.WARNING
        Level::Info => 20,  // logging.INFO
        Level::Debug => 10, // logging.DEBUG
        Level::Trace => 5,
    }
}

// Install the global logger and hand back the sink the client fills in once the server's hello
// confirms it accepts remote logging. Replaces the plain SimpleLogger init in main().
pub fn init(level: LevelFilter) -> LogSink {
    let sink: LogSink = Arc::new(Mutex::new(None));
    let logger = RemoteLogger {
        local: SimpleLogger::new().with_level(level),
        sink: sink.clone(),
    };
    // set_boxed_logger only fails if a logger is already set, which never happens here:
    log::set_boxed_logger(Box::new(logger)).expect("failed to install the logger");
    log::set_max_level(level);
    sink
}
