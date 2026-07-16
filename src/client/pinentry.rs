// Password prompting via pinentry (the GnuPG prompt program), spoken over its Assuan line
// protocol. Used by the client's process_challenge when a `pinentry` binary is on PATH; otherwise
// the built-in AuthDialog is used instead. Kept out of client.rs to keep the state machine there
// readable.
use std::env;
use std::thread;

use log::warn;
use winit::event_loop::EventLoopProxy;

use xpra::net::packet::Packet;

use super::client::client_packet;

// Locate a pinentry binary on PATH (honouring PINENTRY_PROGRAM), so we can offer a native secure
// prompt when one is installed. Returns the full path, or None to fall back to the built-in dialog.
pub fn find_pinentry() -> Option<String> {
    let mut names: Vec<String> = Vec::new();
    if let Ok(prog) = env::var("PINENTRY_PROGRAM") {
        if !prog.is_empty() {
            names.push(prog);
        }
    }
    for name in ["pinentry", "pinentry-mac"] {
        names.push(name.to_string());
    }
    let path = env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) { &["", ".exe"] } else { &[""] };
    for dir in env::split_paths(&path) {
        for name in &names {
            for ext in exts {
                let candidate = dir.join(format!("{name}{ext}"));
                if candidate.is_file() {
                    return Some(candidate.to_string_lossy().into_owned());
                }
            }
        }
    }
    None
}

// Prompt for a password with pinentry on a worker thread (it blocks while the user types), then
// post the outcome back to the UI thread as a synthesized client packet - the challenge equivalent
// of the decode thread's "draw-decoded". An explicit cancel ends authentication; a failure to even
// run pinentry falls back to the built-in dialog.
pub fn spawn_pinentry(prog: String, prompt: String, proxy: EventLoopProxy<Packet>) {
    thread::Builder::new()
        .name("pinentry".to_string())
        .spawn(move || {
            let packet = match run_pinentry(&prog, &prompt) {
                Ok(Some(password)) => client_packet("challenge-password", &password),
                Ok(None) => client_packet("challenge-cancel", ""),
                Err(e) => {
                    warn!("pinentry failed ({e}), falling back to the built-in dialog");
                    client_packet("challenge-fallback-dialog", &prompt)
                }
            };
            let _ = proxy.send_event(packet);
        })
        .unwrap();
}

// Minimal Assuan client for pinentry: read the greeting, set the prompt text, GETPIN. Returns
// Ok(Some(pin)), Ok(None) if the user cancelled, or Err if pinentry could not be driven.
fn run_pinentry(prog: &str, prompt: &str) -> Result<Option<String>, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let mut child = Command::new(prog)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit()) // let pinentry's own errors reach the terminal
        .spawn()
        .map_err(|e| format!("cannot start {prog}: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("no pinentry stdin")?;
    let mut reader = BufReader::new(child.stdout.take().ok_or("no pinentry stdout")?);

    // drive the exchange in a closure so we always reap the child afterwards.
    let result = (|| -> Result<Option<String>, String> {
        // read one Assuan status line, skipping S/# comment and blank lines:
        macro_rules! read_line {
            () => {{
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                        return Err("pinentry closed the connection".to_string());
                    }
                    let l = line.trim_end().to_string();
                    if !(l.starts_with('S') || l.starts_with('#') || l.is_empty()) {
                        break l;
                    }
                }
            }};
        }
        // send a command and require an OK acknowledgement:
        macro_rules! send_ok {
            ($cmd:expr) => {{
                writeln!(stdin, "{}", $cmd).map_err(|e| e.to_string())?;
                let l = read_line!();
                if !(l == "OK" || l.starts_with("OK ")) {
                    return Err(l);
                }
            }};
        }
        // greeting:
        let greeting = read_line!();
        if !(greeting == "OK" || greeting.starts_with("OK ")) {
            return Err(greeting);
        }
        // best-effort option for terminal (curses) pinentry; ignore any rejection:
        if let Ok(tty) = env::var("GPG_TTY") {
            writeln!(stdin, "OPTION ttyname={tty}").map_err(|e| e.to_string())?;
            let _ = read_line!();
        }
        send_ok!("SETTITLE Xpra Authentication");
        send_ok!("SETPROMPT Password:");
        send_ok!(format!("SETDESC {}", assuan_escape(prompt)));
        // GETPIN: an optional "D <pin>" data line followed by OK, or ERR on cancel.
        writeln!(stdin, "GETPIN").map_err(|e| e.to_string())?;
        let mut pin: Option<String> = None;
        loop {
            let l = read_line!();
            if let Some(data) = l.strip_prefix("D ") {
                pin = Some(assuan_unescape(data));
            } else if l == "OK" || l.starts_with("OK ") {
                break;
            } else if l.starts_with("ERR") {
                // any error from GETPIN (typically code 0x5000063, "canceled") = user declined:
                pin = None;
                break;
            }
        }
        let _ = writeln!(stdin, "BYE");
        Ok(pin)
    })();

    drop(stdin);
    let _ = child.wait();
    result
}

// Assuan percent-escaping for text we send (%, CR, LF must be escaped).
fn assuan_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' => out.push_str("%25"),
            '\r' => out.push_str("%0D"),
            '\n' => out.push_str("%0A"),
            _ => out.push(c),
        }
    }
    out
}

// Decode the percent-escaping pinentry applies to the PIN in its "D" data line.
fn assuan_unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
