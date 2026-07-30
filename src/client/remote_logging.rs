use std::cell::Cell;
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Log, Metadata, Record};
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

// The local half of the logger: what `simple_logger` used to print, hand-rolled. Dropping that
// crate takes nine crates out of the graph - `simple_logger` and `colored`, plus the `time` tree
// (`time`, `time-core`, `time-macros`, `deranged`, `num-conv`, `num_threads`, `powerfmt`) that
// its default `timestamps` feature pulled in. `time` was also what forced the whole build onto
// rustc 1.88: every release it allows declares that MSRV, so distributions on an older toolchain
// (Debian trixie ships 1.85) could not build the package at all. Same output as before -
//   2026-07-30T05:29:15.061Z ERROR [xpra] failed to connect
// - on stdout, with the level colourized when stdout is a terminal.
struct LocalLogger {
    level: LevelFilter,
    colors: bool,
}

impl LocalLogger {
    fn new(level: LevelFilter) -> Self {
        // `colored` used to decide this for us: colour only when stdout is a terminal, and never
        // when NO_COLOR is set (https://no-color.org/).
        let colors = std::io::stdout().is_terminal()
            && std::env::var_os("NO_COLOR").is_none()
            && enable_ansi();
        Self { level, colors }
    }

    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let level = format!("{:<5}", record.level());
        let level = if self.colors {
            // the colours `simple_logger` picked, as plain ANSI; Trace was left unstyled.
            match record.level() {
                Level::Error => format!("\x1b[31m{level}\x1b[0m"),
                Level::Warn => format!("\x1b[33m{level}\x1b[0m"),
                Level::Info => format!("\x1b[36m{level}\x1b[0m"),
                Level::Debug => format!("\x1b[35m{level}\x1b[0m"),
                Level::Trace => level,
            }
        } else {
            level
        };
        let target = if record.target().is_empty() {
            record.module_path().unwrap_or_default()
        } else {
            record.target()
        };
        println!("{} {level} [{target}] {}", timestamp(), record.args());
    }
}

// A Windows console does not interpret ANSI escapes until ENABLE_VIRTUAL_TERMINAL_PROCESSING is
// turned on - `colored` did this for us on first use, so without it the escapes above would print
// as literal garbage in conhost. Windows Terminal turns it on itself, but conhost does not.
#[cfg(windows)]
fn enable_ansi() -> bool {
    use windows::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle,
        STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    unsafe {
        let Ok(stdout) = GetStdHandle(STD_OUTPUT_HANDLE) else {
            return false;
        };
        let mut mode = CONSOLE_MODE(0);
        if GetConsoleMode(stdout, &mut mode).is_err() {
            return false;
        }
        SetConsoleMode(stdout, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING).is_ok()
    }
}

#[cfg(not(windows))]
fn enable_ansi() -> bool {
    true
}

// The current time in the ISO-8601 UTC form `simple_logger` printed: 2026-07-30T05:29:15.061Z
fn timestamp() -> String {
    let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since_epoch.as_secs() as i64;
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60,
        since_epoch.subsec_millis()
    )
}

// Days since the epoch -> (year, month, day), by Howard Hinnant's `civil_from_days`
// (http://howardhinnant.github.io/date_algorithms.html, public domain) - the shifted-era algorithm
// every date library uses. Re-basing the year to start on March 1st puts the leap day at the *end*
// of the year, which is what lets the month/day arithmetic below run without a leap-year branch.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468; // re-base 1970-01-01 onto 0000-03-01
    let era = z.div_euclid(146_097); // 146097 days = 400 years = one full leap cycle
    let doe = z - era * 146_097; // day of era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // year of era, [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year (March-based), [0, 365]
    let mp = (5 * doy + 2) / 153; // month, March = 0
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    // January and February belong to the *next* calendar year in this March-based numbering:
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

// A `log::Log` that prints locally (exactly as before) and additionally forwards info-and-above
// records to the xpra server as `logging` packets, so they land in the server log.
struct RemoteLogger {
    local: LocalLogger,
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
        // `println!` only line-buffers when stdout is a terminal, so this is not a no-op (which
        // is all `simple_logger` did here) when the client's output is piped to a file.
        let _ = std::io::stdout().flush();
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
// confirms it accepts remote logging. Called from main() in place of a plain logger init.
pub fn init(level: LevelFilter) -> LogSink {
    let sink: LogSink = Arc::new(Mutex::new(None));
    let logger = RemoteLogger {
        local: LocalLogger::new(level),
        sink: sink.clone(),
    };
    // set_boxed_logger only fails if a logger is already set, which never happens here:
    log::set_boxed_logger(Box::new(logger)).expect("failed to install the logger");
    log::set_max_level(level);
    sink
}

#[cfg(test)]
mod tests {
    use super::civil_from_days;

    #[test]
    fn civil_from_days_vectors() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(59), (1970, 3, 1));
        assert_eq!(civil_from_days(20_664), (2026, 7, 30));
        // pre-epoch, so the day number is negative and the division has to floor, not truncate:
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        // leap years: a plain one, a century that is one (÷400), and a century that is not (÷100).
        // The last two are what a naive implementation gets wrong.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(-25_508), (1900, 3, 1));
        assert_eq!(civil_from_days(47_541), (2100, 3, 1));
    }
}
