use alloc::string::ToString;
use machine_uid;

use std::env;
use std::fmt;
use std::io;
use std::rc::Rc;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{channel, Sender, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use yaml_rust2::Yaml;
use log::{trace, debug, info, warn, error};
use softbuffer::Context;
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy, OwnedDisplayHandle};
use winit::keyboard::{Key, ModifiersState, NamedKey, PhysicalKey};
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::window::{
    CursorGrabMode, CursorIcon, CustomCursor, Fullscreen, Icon, ResizeDirection, Window,
    WindowId, WindowLevel,
};

use xpra::exit_codes::ExitCode;
use xpra::net::serde::VERSION_KEY_STR;
use xpra::VERSION;
use xpra::net::connection::Connection;
use xpra::net::io::{write_packet, read_packet};
use xpra::net::serde::parse_packet;
use xpra::net::packet::{Packet, yaml_hash, yaml_hash_bool, yaml_hash_str, yaml_i32};
use xpra::net::rand::secure_hex;
use xpra::net::sha256::hmac_sha256_hex;
use super::auth_dialog::{AuthDialog, DialogAction};
#[cfg(windows)]
use super::audio::{
    self, AudioProtocol, IncomingAudio, LatencyReporter, OpusHeader,
    AUDIO_CAPABILITIES_PACKET, CODEC,
};
use super::clipboard::start_clipboard_loop;
use super::draw_decoder;
use super::mmap::{self, MmapArea};
use super::pinentry::{find_pinentry, spawn_pinentry};
use super::remote_logging::LogSink;
#[cfg(windows)]
use super::tray;
#[cfg(windows)]
use super::windows_audio::{AudioWorker, EnqueueError};
use super::window::XpraWindow;


// How often we send our own `ping` once the session is up. A few seconds keeps the server's view
// of our latency fresh without being chatty; xpra's own client pings on a similar cadence.
const PING_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Default, PartialEq)]
struct WindowSizeConstraints {
    minimum: Option<(u32, u32)>,
    maximum: Option<(u32, u32)>,
    increment: Option<(u32, u32)>,
}

#[derive(Debug, Default, PartialEq)]
struct WindowMetadataUpdate {
    title: Option<String>,
    decorations: Option<bool>,
    fullscreen: Option<bool>,
    maximized: Option<bool>,
    iconic: Option<bool>,
    above: Option<bool>,
    below: Option<bool>,
    size_constraints: Option<WindowSizeConstraints>,
}

impl WindowMetadataUpdate {
    fn parse(metadata: &Yaml) -> Self {
        WindowMetadataUpdate {
            title: metadata_str(metadata, "title"),
            decorations: metadata_bool(metadata, "decorations"),
            fullscreen: metadata_bool(metadata, "fullscreen"),
            maximized: metadata_bool(metadata, "maximized"),
            iconic: metadata_bool(metadata, "iconic"),
            above: metadata_bool(metadata, "above"),
            below: metadata_bool(metadata, "below"),
            size_constraints: metadata_hash(metadata, "size-constraints").map(|constraints| {
                WindowSizeConstraints {
                    minimum: metadata_pair(constraints, "minimum-size"),
                    maximum: metadata_pair(constraints, "maximum-size"),
                    increment: metadata_pair(constraints, "increment"),
                }
            }),
        }
    }
}

fn metadata_hash<'a>(metadata: &'a Yaml, key: &str) -> Option<&'a Yaml> {
    let Yaml::Hash(hash) = metadata else {
        return None;
    };
    let value = hash.get(&Yaml::String(key.to_string()))?;
    matches!(value, Yaml::Hash(_)).then_some(value)
}

fn metadata_str(metadata: &Yaml, key: &str) -> Option<String> {
    let Yaml::Hash(hash) = metadata else {
        return None;
    };
    match hash.get(&Yaml::String(key.to_string())) {
        Some(Yaml::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn metadata_bool(metadata: &Yaml, key: &str) -> Option<bool> {
    let Yaml::Hash(hash) = metadata else {
        return None;
    };
    match hash.get(&Yaml::String(key.to_string())) {
        Some(Yaml::Boolean(value)) => Some(*value),
        Some(Yaml::Integer(value)) => Some(*value != 0),
        _ => None,
    }
}

fn metadata_pair(metadata: &Yaml, key: &str) -> Option<(u32, u32)> {
    let Yaml::Hash(hash) = metadata else {
        return None;
    };
    let Some(Yaml::Array(values)) = hash.get(&Yaml::String(key.to_string())) else {
        return None;
    };
    if values.len() < 2 {
        return None;
    }
    let (Yaml::Integer(width), Yaml::Integer(height)) = (&values[0], &values[1]) else {
        return None;
    };
    if *width <= 0 || *height <= 0 || *width > u32::MAX as i64 || *height > u32::MAX as i64 {
        return None;
    }
    Some((*width as u32, *height as u32))
}

// One local monitor, in the terms xpra's `monitors` capability describes them (see
// `validated_monitor_data`, xpra util/parsing.py, for the full set of attributes it accepts - these
// are the ones winit can answer for).
pub struct MonitorInfo {
    pub name: String,
    pub primary: bool,
    // x, y, width, height in *physical* pixels. x/y may be negative: a monitor placed left of or
    // above the primary one has negative coordinates on Windows.
    pub geometry: (i32, i32, u32, u32),
    // milli-hertz - so a 60Hz panel is 60000, not 60 - which is the unit xpra's monitor definitions
    // use ("value is pre-multiplied by 1000", `get_client_refresh_rate` in xpra
    // server/subsystem/display.py, which divides by 1000 again to get Hz) and, conveniently, the one
    // winit reports in. `None` when winit does not know the mode's refresh rate.
    pub refresh_rate_millihertz: Option<u32>,
}

// Every monitor winit knows about, in its own order - which is the order xpra indexes them by.
//
// Two attributes of xpra's monitor definitions are deliberately never filled in. `width-mm`/
// `height-mm`: winit exposes no physical dimensions, and inventing them from an assumed DPI would
// feed the server's DPI heuristics a fabricated number. `scale-factor`: xpra's own client reports
// GDK's *logical* geometry alongside an integer scale, whereas everything here - these geometries,
// `desktop_size`, and every window rectangle this client handles - is in physical pixels, so a
// scale factor would only invite the server to apply it twice.
//
// The list can legitimately come back empty (some Wayland compositors, a headless X11 display), and
// `primary` is always false on Wayland, where winit's `primary_monitor` returns nothing by design.
fn local_monitors(event_loop: &ActiveEventLoop) -> Vec<MonitorInfo> {
    let primary = event_loop.primary_monitor();
    let mut monitors = Vec::new();
    for monitor in event_loop.available_monitors() {
        let size = monitor.size();
        if size.width == 0 || size.height == 0 {
            // a monitor with no current video mode tells us nothing and would only confuse the
            // bounding box below.
            continue;
        }
        let position = monitor.position();
        monitors.push(MonitorInfo {
            // xpra generates a name from the index when we send none, but winit's is better when
            // there is one (the connector name on X11/Wayland, the device name on Windows).
            name: monitor.name().unwrap_or_default(),
            primary: Some(&monitor) == primary.as_ref(),
            geometry: (position.x, position.y, size.width, size.height),
            refresh_rate_millihertz: monitor.refresh_rate_millihertz(),
        });
    }
    monitors
}

// The monitor-relative form of an absolute point, as xpra's `MonitorLayout.relative_position`
// computes it (xpra util/screen.py): the index of the monitor the point falls in, and the point
// rebased against that monitor's top-left corner. `None` when no monitor contains it - which
// includes having no monitor list at all (Wayland compositors that enumerate none, a headless X11
// display), and the dead space a non-rectangular multi-monitor arrangement leaves between panels.
//
// This is what lets the server place a pointer or a window where the user actually sees it. The
// absolute coordinates we send are raw and can be negative (a monitor left of or above the primary
// one on Windows), whereas the server rebases the layout we sent in `hello` to a non-negative
// origin before mirroring it onto its virtual screen (`normalized_monitors`) - so it cannot map our
// absolute coordinates back on its own. Given the index and the offset within that monitor it can:
// `get_monitor_position` (xpra server/source/display.py) resolves the pair against its *normalized*
// copy of the same layout, and the coordinate spaces line up again.
fn monitor_relative_position(monitors: &[MonitorInfo], x: i32, y: i32) -> Option<(usize, i32, i32)> {
    for (index, monitor) in monitors.iter().enumerate() {
        let (mx, my, w, h) = monitor.geometry;
        // the width/height are u32 and the coordinates i32, so compare in i64: a monitor at
        // i32::MAX-ish coordinates would otherwise wrap.
        let (x64, y64, mx64, my64) = (x as i64, y as i64, mx as i64, my as i64);
        if x64 >= mx64 && x64 < mx64 + w as i64 && y64 >= my64 && y64 < my64 + h as i64 {
            return Some((index, x - mx, y - my));
        }
    }
    None
}

// The total size of the local display area, in physical pixels: the bounding box of every monitor,
// which is the "virtual screen" on Windows and the root window size on X11 (both of which is what
// xpra's own client reports as `desktop_size`). The box is measured min-to-max rather than from the
// origin, because of those negative coordinates.
//
// `None` when there is no monitor to measure, or when the result falls outside what the server will
// accept - it rejects anything above 32767 as invalid (`parse_client_caps`, xpra
// server/source/display.py), and would then have no size at all, so send none rather than a bogus
// one.
fn total_display_size(monitors: &[MonitorInfo]) -> Option<(u32, u32)> {
    let mut bounds: Option<(i64, i64, i64, i64)> = None;
    for monitor in monitors {
        let (x, y, w, h) = monitor.geometry;
        let (left, top) = (x as i64, y as i64);
        let (right, bottom) = (left + w as i64, top + h as i64);
        bounds = Some(match bounds {
            None => (left, top, right, bottom),
            Some((l, t, r, b)) => (l.min(left), t.min(top), r.max(right), b.max(bottom)),
        });
    }
    let (left, top, right, bottom) = bounds?;
    let (width, height) = (right - left, bottom - top);
    if width <= 0 || height <= 0 || width >= 32768 || height >= 32768 {
        warn!("not reporting the invalid local display size {width}x{height}");
        return None;
    }
    Some((width as u32, height as u32))
}


pub struct XpraClient {
    pub hello_sent: bool,
    pub server_version: String,
    // The server lists the packet types it accepts when we request them in `hello`. A server in
    // backwards-compatible mode includes the legacy `damage-sequence` alias; older servers which
    // do not return the list are assumed to be compatible, since that remains xpra's default.
    pub server_backwards_compatible: bool,
    pub windows: HashMap<u64, XpraWindow>,
    pub id_map: HashMap<WindowId, u64>,
    pub stream: Connection,
    pub proxy: EventLoopProxy<Packet>,
    pub decode_sender: Sender<Packet>,
    pub softbuffer_ctx: Option<Context<OwnedDisplayHandle>>,
    pub modifiers: ModifiersState,
    pub startup_complete: bool,
    // monotonic clock base for the timestamps in our own `ping` packets. The server echoes the
    // value back untouched, so subtracting it from `start.elapsed()` on the echo recovers the
    // round-trip time (both measured with this one clock, so absolute epoch is irrelevant).
    pub start: Instant,
    // the last client->server round-trip we measured from a `ping_echo`, in milliseconds (-1 until
    // the first echo). This is what we report back in the `ping_echo` packets we send in reply to
    // the server's own pings - the channel by which the server learns our network latency.
    pub last_client_latency_ms: i64,
    // the current pointer cursor (xpra sends one cursor for the whole session, not per-window);
    // kept so it can be applied to windows created after the last "cursor" packet. `None` = the
    // platform default cursor.
    pub current_cursor: Option<CustomCursor>,
    // the local monitors and the total size of the display area they span (in physical pixels),
    // both sent to the server in `hello` - see `local_monitors` / `total_display_size` and
    // `send_hello`. Filled in from `resumed`, which is the first callback that hands us an
    // `ActiveEventLoop` to enumerate monitors with. `desktop_size` is `None` when winit reports no
    // usable monitor, in which case we send no size at all rather than a bogus one.
    pub monitors: Vec<MonitorInfo>,
    pub desktop_size: Option<(u32, u32)>,
    // the window whose pointer is currently grabbed at the server's request. The grab is applied
    // through winit and must be explicitly released on pointer-ungrab or before that window is
    // destroyed.
    pub pointer_grabbed: Option<u64>,
    // the in-app password prompt shown when a server sends a `challenge` and no pinentry is
    // available (see process_challenge); `None` when we are not prompting.
    pub auth_dialog: Option<AuthDialog>,
    // the server salt from the challenge we are currently answering, held while an interactive
    // prompt (pinentry worker or the dialog) is collecting the password. `None` when not
    // authenticating. Only `hmac+sha256` is advertised/handled, so this salt is all we need.
    pub pending_challenge: Option<Vec<u8>>,
    // `Some` once we're on the way out (a `disconnect` packet, a lost connection or a failed
    // write): it holds the code we'll exit the process with, and stops us from writing to (and
    // complaining about) a dead connection while the event loop winds down.
    pub exit_code: Option<ExitCode>,
    // shared with the global logger (see remote_logging.rs): we drop our `EventLoopProxy` into it
    // once the server's hello confirms it accepts client logs, which switches on forwarding of
    // info-and-above records to the server as `logging` packets. Empty otherwise.
    pub log_sink: LogSink,
    // plain-text clipboard sync (see clipboard.rs). `Some` once the server's hello advertised
    // clipboard support, at which point the clipboard thread is running; the `Sender` hands it text
    // the remote end copied, to place on the local OS clipboard. `None` = server has no clipboard.
    pub clipboard: Option<Sender<String>>,
    // whether clipboard syncing is currently on. Set when the thread starts, toggled by the
    // server's `set-clipboard-enabled`; gates both directions while the thread stays alive.
    pub clipboard_enabled: bool,
    // the last plain-text value synced in either direction, used to break the copy<->paste feedback
    // loop: a value we just wrote locally is not re-sent to the server, and vice-versa.
    pub last_clipboard: String,
    // the connection string as the user typed it, kept to name the session in the system tray's
    // tooltip and menu header. Only read on Windows, which is the only platform with a tray.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub target: String,
    // credentials collected by the connection dialog (see connect_dialog.rs), when the client was
    // started without a target on the command line. `username` overrides the one we would otherwise
    // take from the environment in `hello`; `password` answers an authentication challenge without
    // prompting a second time (see process_challenge). Both `None` on the command-line path.
    pub username: Option<String>,
    pub password: Option<String>,
    // the shared memory area the server writes pixels into when it runs on this same host (see
    // mmap.rs). Created before the connection, offered in our `hello` and confirmed - or not - by
    // the server's reply; the decode thread holds the other `Arc`. `None` when mmap is switched
    // off, unsupported, or could not be set up.
    pub mmap: Option<Arc<MmapArea>>,
    // the Windows notification-area icon and its "Exit" menu (see tray.rs), created in `resumed`
    // and removed when this client is dropped. `None` if the tray could not be created, which is
    // not fatal - the client just has no tray.
    #[cfg(windows)]
    pub tray: Option<tray::Tray>,
    // Windows speaker forwarding. The handle only owns a bounded command sender; every COM,
    // Media Foundation and WASAPI object remains on the worker thread.
    #[cfg(windows)]
    pub audio_worker: Option<AudioWorker>,
    #[cfg(windows)]
    pub audio_protocol: AudioProtocol,
    #[cfg(windows)]
    pub audio_sync_reporter: LatencyReporter,
    #[cfg(windows)]
    pub audio_queue_warned: bool,
}


// "connection-lost" and "invalid-packet" are client-side packet types (like "draw-decoded"):
// the reader thread and the write path use them to tell the UI thread that the connection is
// gone, since only `user_event` has access to the `ActiveEventLoop` needed to stop the event loop.
pub(crate) fn client_packet(packet_type: &str, message: &str) -> Packet {
    Packet {
        main: vec![
            Yaml::String(packet_type.to_string()),
            Yaml::String(message.to_string()),
        ],
        raw: HashMap::new(),
        decode_time_us: None,
    }
}

// xpra's `disconnect_is_an_error` (`net/common.py`): disconnect reasons are free-form strings
// (`ConnectionMessage` in `net/constants.py`), and an error is anything that says "error", or any
// timeout other than the idle one.
fn disconnect_is_an_error(reason: &str) -> bool {
    reason.contains("error") || (reason.contains("timeout") && reason != "idle timeout")
}

// Read one `mmap` draw packet: instead of pixel data it carries (offset, length) pairs into the
// shared memory area the server has been writing frames into (see mmap.rs). Runs on the decode
// thread, in place of a decoder, and hands back the same tightly packed BGRX buffer the real
// decoders produce.
fn read_mmap_draw(packet: &Packet, area: &MmapArea) -> Result<Vec<u8>, String> {
    let options = packet.main.get(10);
    // the chunk list rides in the packet options; the server also leaves a copy in the packet's
    // data field, which is the only place older clients look (paint_mmap, xpra
    // client/gui/window/backing.py), so fall back to it.
    let list = match options.and_then(|o| yaml_hash(o, "chunks")).or_else(|| packet.main.get(7)) {
        Some(list) => list,
        None => return Err("mmap draw packet without a chunk list".to_string()),
    };
    let chunks = mmap::parse_chunks(list)?;
    let pixels = read_mmap_pixels(packet, area, options, &chunks);
    // Whatever happened above: the server does not reclaim this part of the ring until we move
    // `data_start` past it, so dropping a draw without releasing it would stall the session.
    area.release(&chunks);
    pixels
}

fn read_mmap_pixels(packet: &Packet, area: &MmapArea, options: Option<&Yaml>,
                    chunks: &[(usize, usize)]) -> Result<Vec<u8>, String> {
    // we advertise `encoding.rgb_formats = ["BGRX"]` and nothing else, so this is what the server
    // writes - BGRA would do just as well (we ignore the alpha byte), anything else would not.
    let rgb_format = options.map(|o| yaml_hash_str(o, "rgb_format".to_string())).unwrap_or_default();
    if !rgb_format.is_empty() && !rgb_format.starts_with("BGR") {
        return Err(format!("unsupported mmap pixel format {:?}", rgb_format));
    }
    // The source stride, which for a damage sub-rectangle is the whole window's rather than w*4.
    // This is the only place a draw packet's rowstride is read: every other encoding we handle
    // produces tightly packed output.
    let rowstride = packet.main.get(9).map(yaml_i32).unwrap_or(0).max(0) as usize;
    let w = packet.get_i32(4).max(0) as usize;
    let h = packet.get_i32(5).max(0) as usize;
    area.read_image(chunks, w, h, rowstride)
}

fn connection_error(e: &io::Error) -> String {
    match e.kind() {
        // what a killed server looks like: the socket closed mid-packet (or between packets),
        // and "failed to fill whole buffer" is not a helpful thing to show the user.
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset =>
            "connection closed by the server".to_string(),
        _ => e.to_string(),
    }
}

// Ring an audible bell. winit has no bell primitive, so this is per-platform and best-effort:
// Windows plays a real tone honouring the server's pitch/duration; elsewhere we emit the terminal
// BEL, which beeps only if the client was launched from a terminal with an audible bell - there is
// no portable desktop bell without an X11/audio dependency (see README).
fn ring_bell(pitch: i32, duration: i32) {
    #[cfg(windows)]
    {
        // Beep() is valid for 37..=32767 Hz and blocks for `duration` ms, so fall back to a sane
        // default tone for the X11 "server default" (pitch 0) and ring on a throwaway thread.
        let freq = if (37..=32767).contains(&pitch) { pitch as u32 } else { 800 };
        let dur = if duration > 0 { (duration as u32).min(5000) } else { 100 };
        thread::spawn(move || {
            let _ = unsafe { windows::Win32::System::Diagnostics::Debug::Beep(freq, dur) };
        });
    }
    #[cfg(not(windows))]
    {
        let _ = (pitch, duration);
        use std::io::Write;
        let mut stderr = io::stderr();
        let _ = stderr.write_all(b"\x07");
        let _ = stderr.flush();
    }
}

fn draw_ack_packet(backwards_compatible: bool, packet_sequence: u64, wid: u64,
                   width: u32, height: u32, decode_time: i128, message: String) -> Value {
    if backwards_compatible {
        json!([
            "window-draw-ack", packet_sequence, wid, width, height, decode_time, message,
        ])
    } else {
        json!([
            "window-ack", wid, width, height, packet_sequence, decode_time, message,
        ])
    }
}

fn server_backwards_compatible(hello: &Yaml) -> Option<bool> {
    let Yaml::Hash(hash) = hello else {
        return None;
    };
    let Some(Yaml::Array(packet_types)) =
        hash.get(&Yaml::String("packet-types".to_string()))
    else {
        return None;
    };
    let has_packet_type = |name: &str| packet_types.iter()
        .any(|value| matches!(value, Yaml::String(s) if s == name));
    has_packet_type("window-ack").then(|| has_packet_type("damage-sequence"))
}

impl fmt::Debug for XpraClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XpraClient")
            .field("server", &self.server_version)
            .finish()
    }
}

impl XpraClient {

    pub fn new(stream: Connection, proxy: EventLoopProxy<Packet>, decode_sender: Sender<Packet>,
               log_sink: LogSink, target: String, mmap: Option<Arc<MmapArea>>) -> Self {
        #[cfg(windows)]
        let audio_worker = match AudioWorker::start(proxy.clone()) {
            Ok(worker) => Some(worker),
            Err(error) => {
                // One warning only: speaker forwarding is optional and the rest of the session is
                // fully usable when Media Foundation or the default endpoint is unavailable.
                warn!("speaker forwarding unavailable: {error}");
                None
            }
        };
        XpraClient {
            hello_sent: false,
            server_version: "".to_string(),
            server_backwards_compatible: true,
            windows: HashMap::new(),
            id_map: HashMap::new(),
            stream,
            proxy,
            decode_sender,
            softbuffer_ctx: None,
            modifiers: ModifiersState::empty(),
            startup_complete: false,
            start: Instant::now(),
            last_client_latency_ms: -1,
            current_cursor: None,
            monitors: Vec::new(),
            desktop_size: None,
            pointer_grabbed: None,
            auth_dialog: None,
            pending_challenge: None,
            exit_code: None,
            log_sink,
            clipboard: None,
            clipboard_enabled: false,
            last_clipboard: String::new(),
            target,
            username: None,
            password: None,
            mmap,
            #[cfg(windows)]
            tray: None,
            #[cfg(windows)]
            audio_worker,
            #[cfg(windows)]
            audio_protocol: AudioProtocol::default(),
            #[cfg(windows)]
            audio_sync_reporter: LatencyReporter::default(),
            #[cfg(windows)]
            audio_queue_warned: false,
        }
    }

    // stop the event loop, remembering what to exit the process with (the first cause wins).
    fn quit(&mut self, event_loop: &ActiveEventLoop, exit_code: ExitCode) {
        if self.exit_code.is_none() {
            self.exit_code = Some(exit_code);
        }
        event_loop.exit();
    }

    // losing the connection before the session is up means we never had a usable server
    // (wrong port, not an xpra server, rejected before `startup-complete`, ...), which xpra
    // reports as `CONNECTION_FAILED` rather than `CONNECTION_LOST`.
    fn connection_lost_code(&self) -> ExitCode {
        if self.startup_complete { ExitCode::ConnectionLost } else { ExitCode::ConnectionFailed }
    }

    // Send our `hello`. `reply` is `Some((challenge_response, client_salt))` on the second hello,
    // once we've answered a server `challenge` (see process_challenge); `None` on the first one.
    pub fn send_hello(&mut self, reply: Option<(String, String)>) {
        let platform = match std::env::consts::OS {
            "windows" => "win32",
            "macos" => "darwin",
            other => other,
        };
        // the username typed in the connection dialog wins over the one we are running as: it is
        // the account the *session* belongs to, which is what a server authenticating per-user
        // matches against (and what its password prompt names).
        let env_username = env::var("USERNAME").or_else(|_| env::var("USER")).unwrap_or_default();
        let username = self.username.clone().unwrap_or(env_username);
        // h264 is decoded via Media Foundation, which is Windows-only:
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut encodings = vec!["jpeg", "png", "webp"];
        // The nested "encoding" caps dict (read server-side as hello["encoding"], see xpra's
        // server/source/encoding.py). For a video encoding to be offered at all, the server needs
        // `full_csc_modes[<enc>]` to list at least one colourspace its encoder can produce that we
        // can decode. Media Foundation's H.264 decoder only handles 8-bit 4:2:0 up to High profile,
        // so we advertise *only* YUV420P (never 422/444) and pin the profile to "high".
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut encoding_caps = json!({
            // read server-side as hello["encoding"]["window-icon"]; without it the server
            // sends no "window-icon" packets at all (it only ships icons as png).
            "window-icon": ["png"],
        });
        #[cfg(windows)]
        {
            encodings.push("h264");
            encoding_caps["full_csc_modes"] = json!({ "h264": ["YUV420P"] });
            encoding_caps["h264"] = json!({ "YUV420P.profile": "high" });
        }
        // the picture encodings we can decode (parse_encoding_caps, xpra server/source/encoding.py).
        // The two lists are identical here: every encoding we advertise is one draw_decoder.rs
        // handles directly, none is a container for another.
        encoding_caps["options"] = json!(encodings);
        encoding_caps["core"] = json!(encodings);
        if self.mmap.is_some() {
            // the raw pixel layouts we accept, which is what the server writes into the mmap area
            // (mmap_encode, xpra server/window/compress.py). This is *not* optional: the server
            // defaults to ("RGB",) - three bytes per pixel - which window::paint cannot render.
            // BGRX is both what it happens to want and X11's native little-endian layout, so the
            // server ends up doing no conversion at all. We list no alpha format: our framebuffer
            // is opaque 0x00RRGGBB, and advertising only BGRX makes the server flatten any window
            // that has an alpha channel for us.
            encoding_caps["rgb_formats"] = json!(["BGRX"]);
        }
        // The nested "display" caps dict (xpra server/source/display.py, DisplayConnection). Sending
        // it is what instantiates that subsystem server-side at all: `is_needed` looks for this key
        // and, failing that, for the pre-6.5 spelling where these attributes were flattened into the
        // top level - which it only accepts in backwards-compatible mode, so like the packet renames
        // the legacy form is dead weight and is not sent. Watch out for the flattened keys the
        // subsystem still reads from the *top* level when this dict is absent (`show-desktop`
        // below): once the dict is present, only what is inside it is read.
        let mut display_caps = json!({
            // let the server forward "show the desktop" requests (EWMH _NET_SHOWING_DESKTOP); it
            // only sends `show-desktop` packets if we advertise this. We honour them by minimizing
            // / restoring our windows (see process_show_desktop).
            "show-desktop": true,
            // we have no way to resize the local display, so the server's notifications that *its*
            // root window changed size (the legacy `desktop_size` packet) are of no use to us.
            "resize-events": false,
        });
        // the total size of the local display area, which the server logs as "client total display
        // size" and - on a seamless server that can resize its virtual screen - adopts as the size
        // of that screen (`do_parse_screen_info` / `configure_best_screen_size`, xpra
        // server/subsystem/display.py), so that remote windows are laid out for a desktop we can
        // actually show them on.
        if let Some((w, h)) = self.desktop_size {
            display_caps["desktop_size"] = json!([w, h]);
        }
        // ... and its breakdown into individual monitors. An X11 server whose dummy driver has
        // RandR 1.6 goes one better than resizing: it reproduces this layout as real virtual
        // monitors (`mirror_client_monitor_layout` -> `set_crtc_config`, xpra
        // x11/subsystem/display.py), so remote applications maximize and snap to the same edges the
        // user sees locally. Everywhere else it is what makes per-monitor window placement possible
        // (`MonitorLayout`, xpra util/screen.py).
        //
        // The keys are the monitor indices as strings, which is all our JSON-as-YAML writer can
        // emit; the server puts them back through `int()` (`validated_monitor_data`). Geometries go
        // out with their raw - possibly negative - coordinates, which the server rebases itself
        // (`get_normalized_monitor_definitions`).
        if !self.monitors.is_empty() {
            let mut monitors = json!({});
            for (index, monitor) in self.monitors.iter().enumerate() {
                let (x, y, w, h) = monitor.geometry;
                let mut mdef = json!({
                    "geometry": [x, y, w, h],
                    "primary": monitor.primary,
                });
                if !monitor.name.is_empty() {
                    mdef["name"] = json!(monitor.name);
                }
                if let Some(rate) = monitor.refresh_rate_millihertz {
                    mdef["refresh-rate"] = json!(rate);
                }
                monitors[index.to_string()] = mdef;
            }
            display_caps["monitors"] = monitors;
        }
        let mut packet = json!(["hello", {
            "version": VERSION,
            // Needed to distinguish the legacy draw acknowledgement layout from `window-ack`.
            // Backwards-compatible servers include the `damage-sequence` alias in this list.
            "wants": ["packet-types"],
            // the packet encoders we can read, negotiated against the server's own list
            // (enable_encoder_from_caps, xpra net/protocol/socket_handler.py).
            "encoders": ["yaml"],
            // out-of-band chunks: let the server send large binary items (pixel data, window
            // icons, cursors, clipboard payloads) as their own packets instead of base64-inlining
            // them in the YAML payload - which costs a third more bytes and a decode pass on our
            // side. See net::io's read_packet for the reassembly.
            "chunks": true,
            // packet compression: advertise lz4 (the only algorithm we decompress, see net::io)
            // and a non-zero level so the server actually compresses its packets to us - it falls
            // back to "none" when compression_level is 0 (xpra server/core.py). This is inbound
            // only; our own outgoing packets are small input events, sent uncompressed.
            "compressors": ["lz4"],
            "compression_level": 1,
            "windows": true,
            "keyboard": true,
            "mouse": true,
            "sharing": true,
            "bell": true,
            "display": display_caps,
            // authentication: we only implement the hmac+sha256 password digest, so advertise
            // just that. The server picks the challenge digest from these lists (choose_digest,
            // xpra auth/sys_auth_base.py), so listing one forces both to hmac+sha256 - the one
            // hash we compute in process_challenge / net::sha256.
            "digest": ["hmac+sha256"],
            "salt-digest": ["hmac+sha256"],
            // request pointer cursor forwarding. We only advertise the "png" cursor encoding
            // (decoded like window icons); "backwards-compatible" makes the server send the old
            // "cursor" packet, matching the rest of this client. The legacy "cursors" bool is a
            // fallback for how older servers gate cursor sending.
            "cursor": { "encodings": ["png"], "backwards-compatible": true },
            "cursors": true,
            // allow remote applications to confine the local pointer to their forwarded window.
            // The server only emits pointer-grab / pointer-ungrab when this nested capability is
            // present (server/source/window.py).
            "pointer": { "grabs": true },
            // advertise only the window metadata keys we actually apply. Without this list, the
            // server assumes the broad legacy default and sends properties this client ignores.
            "metadata": {
                "supported": [
                    "title", "size-constraints", "fullscreen", "maximized", "iconic",
                    "decorations", "above", "below",
                ],
            },
            // desktop notifications: shown as balloons on the system tray icon on Windows, logged
            // everywhere else (see process_notify_show). The server gates notification sending on
            // this dict's "enabled" flag.
            "notifications": { "enabled": true },
            // receive informational server lifecycle events such as "handshake-complete",
            // "startup-complete", "suspend", "resume" and "exit". Dedicated protocol packets
            // remain authoritative; server-event packets are logged for diagnostics only.
            "events": true,
            // plain-text clipboard sync (see clipboard.rs and process_hello). The server reads this
            // as hello["clipboard"] (a non-empty dict is what enables clipboard at all - xpra
            // server/source/clipboard.py). `greedy` makes it ship the copied text inside the token
            // it sends us, so a remote copy needs no extra request round-trip; `want_targets` asks
            // for the target list. We scope to just the CLIPBOARD selection and the text targets -
            // no PRIMARY/SECONDARY, no images/files.
            "clipboard": {
                "enabled": true,
                "greedy": true,
                "want_targets": true,
                "selections": ["CLIPBOARD"],
                "preferred-targets": ["UTF8_STRING", "STRING", "text/plain"],
            },
            "ping": true,
            "encoding": encoding_caps,
            "client_type": "rust",
            "platform": platform,
            "user": env::var("USER").unwrap_or("".into()),
            "username": username,
            "hostname": env::var("HOSTNAME").unwrap_or("".into()),
            "uuid": machine_uid::get().unwrap(),
        }]);
        if let Some((response, client_salt)) = reply {
            // both are ASCII (response is hex; we deliberately use a hex client_salt too) so they
            // survive our JSON-as-YAML writer without any binary encoding - see process_challenge.
            let caps = &mut packet[1];
            caps["challenge_response"] = json!(response);
            caps["challenge_client_salt"] = json!(client_salt);
            // padding so a passive observer can't infer the password length from the packet size
            // (only meaningful over ssl/wss); a throwaway random-hex string, like xpra's own.
            caps["challenge_padding"] = json!(secure_hex(64));
        }
        // Offer the shared memory area for the server to write pixels into (see mmap.rs). The
        // prefix is from *our* point of view: our "read" area is the server's write area, which is
        // how it looks the capability up (`tdcaps.dictget("read")`, xpra server/source/mmap.py).
        // Prefixed only: the read/write split landed in xpra 6.3, below the 6.4 protocol we
        // announce, so the unprefixed legacy form would be dead weight.
        if let Some(area) = &self.mmap {
            packet[1]["mmap"] = json!({ "read": area.caps() });
        }
        // Audio probing happened before this hello was built. Advertise only the asynchronous
        // request here; the decoder list is sent later in `audio-capabilities`.
        #[cfg(windows)]
        if self.audio_worker.is_some() {
            packet[1]["audio"] = audio::hello_capabilities();
            packet[1]["av-sync"] = audio::av_sync_capabilities();
        }
        self.write_json(packet);
    }

    pub fn send_focus(&mut self, wid: u64) {
        let packet = json!(["window-focus", wid]);
        self.write_json(packet);
    }

    // The `monitor` descriptor xpra puts next to an absolute position - `{"index", "position"}`,
    // the monitor the point falls in and its offset within it (see `monitor_relative_position` for
    // why the server needs it). `None` when the point is on no known monitor, in which case the
    // caller leaves the key out and the server keeps using the absolute coordinates it was sent
    // alongside.
    fn monitor_descriptor(&self, x: i32, y: i32) -> Option<Value> {
        let (index, mx, my) = monitor_relative_position(&self.monitors, x, y)?;
        Some(json!({ "index": index, "position": [mx, my] }))
    }

    // The same descriptor for a *window* origin. A window's top-left corner is regularly outside
    // every monitor - dragged past the left or top edge of its own screen - so the plain
    // containing-monitor lookup would drop the descriptor at exactly the positions the server most
    // needs help with. winit still knows which monitor the window is on, so ask it first and rebase
    // against that one; the resulting offset may be negative, which is fine, since the server only
    // adds it back to the monitor's origin (`MonitorLayout.position`). This mirrors what xpra's own
    // client does with `get_monitor_at_window` (client/gtk3/window/base.py get_monitor_position).
    //
    // Which monitor we name does not have to be the "right" one for the result to be right: the
    // server resolves the pair as `(monitor origin) + (offset)`, and the offset is measured against
    // that same monitor, so the absolute point it lands on is the same whichever one we pick. The
    // preference only buys a descriptor where the containment test has none to give.
    fn window_monitor_descriptor(&self, wid: u64, x: i32, y: i32) -> Option<Value> {
        let current = self
            .windows
            .get(&wid)
            .and_then(|window| window.window.current_monitor())
            .and_then(|monitor| {
                // winit hands back a fresh `MonitorHandle`, so match it to the list we sent in
                // `hello` by geometry - two monitors cannot share a rectangle, and it is the only
                // attribute both sides are guaranteed to agree on (a name can be missing).
                let (position, size) = (monitor.position(), monitor.size());
                let geometry = (position.x, position.y, size.width, size.height);
                self.monitors.iter().position(|m| m.geometry == geometry)
            });
        match current {
            Some(index) => {
                let (mx, my, _, _) = self.monitors[index].geometry;
                Some(json!({ "index": index, "position": [x - mx, y - my] }))
            }
            None => self.monitor_descriptor(x, y),
        }
    }

    // The properties dict of a pointer packet. `monitor` is the only key we can fill in: the
    // absolute pair is already the packet's own pointer field, and `window-position` would be the
    // position within the window, which the caller has converted away by this point.
    fn pointer_props(&self, x: i32, y: i32) -> Value {
        let mut props = json!({});
        if let Some(monitor) = self.monitor_descriptor(x, y) {
            props["monitor"] = monitor;
        }
        props
    }

    fn send_pointer_position(&mut self, wid: u64, x: i32, y: i32) {
        let device_id = 0;
        let sequence = 0;
        let packet = json!(["pointer-motion", device_id, sequence, wid, [x, y], self.pointer_props(x, y)]);
        self.write_json(packet);
    }

    fn send_pointer_button(&mut self, wid: u64, button: i8, pressed: bool, x: i32, y: i32) {
        let device_id = 0;
        let sequence = 0;
        let props = self.pointer_props(x, y);
        let packet = json!(["pointer-button", device_id, sequence, wid, button, pressed, [x, y], props]);
        self.write_json(packet);
    }

    // `keyboard-event` replaced the positional `key-action` packet: everything after `pressed` is
    // now a single attributes dict, so a client can leave out what it doesn't know (the server
    // defaults each key - xpra server/subsystem/keyboard.py do_process_keyboard_event). We fill in
    // the same five keys xpra's own clients send; `keyval` stays 0 because we derive the keyname
    // from winit rather than from an X11 keysym.
    fn send_key_event(&mut self, wid: u64, keycode: u32, keyname: &str, keystr: &str, pressed: bool) {
        let modifiers = self.get_modifier_state();
        let group = 0;
        let packet = json!(["keyboard-event", wid, keyname, pressed, {
            "modifiers": modifiers,
            "keyval": 0,
            "string": keystr,
            "keycode": keycode,
            "group": group,
        }]);
        self.write_json(packet);
    }

    fn get_modifier_state(&self) -> Vec<String> {
        let mut modifiers: Vec<String> = Vec::new();
        if self.modifiers.shift_key() {
            modifiers.push("shift".to_string());
        }
        if self.modifiers.control_key() {
            modifiers.push("control".to_string());
        }
        if self.modifiers.alt_key() {
            modifiers.push("mod1".to_string());
        }
        modifiers
    }

    // `window-map` stayed positional, so the monitor descriptor is an *optional trailing field*
    // (index 8) rather than a dict key: the server reads it only when the packet is long enough
    // (`len(packet) >= 9`, xpra x11/subsystem/window.py _process_window_map), which is why it is
    // appended rather than sent as a null placeholder. It goes through `resolve_monitor_geometry`
    // there and replaces the x,y of the geometry that follows it. The x,y we are given here is the
    // position `process_new_common` asked winit to place the window at, so it is a local position -
    // which is what the descriptor has to describe.
    fn send_window_map(&mut self, wid: u64, x: i32, y: i32, w: u32, h: u32) {
        let mut packet = json!(["window-map", wid, x, y, w, h, {}, {}]);
        if let Some(monitor) = self.window_monitor_descriptor(wid, x, y) {
            // json! builds an array here, so this cannot fail.
            if let Some(fields) = packet.as_array_mut() {
                fields.push(monitor);
            }
        }
        self.write_json(packet);
    }

    // `window-configure` replaced the positional `configure-window` packet: the geometry, client
    // properties, window state and pointer data all moved into one dict, keyed by name, and every
    // key is optional (xpra server/subsystem/window.py _process_window_configure). We have a
    // geometry to report and, when we can place its origin on a local monitor, the monitor-relative
    // form of that origin - omitting "state"/"properties" is the same as the empty dicts the old
    // packet had to carry.
    fn send_window_configure(&mut self, wid: u64, x: i32, y: i32, w: u32, h: u32) {
        let mut config = json!({ "geometry": [x, y, w, h] });
        if let Some(monitor) = self.window_monitor_descriptor(wid, x, y) {
            config["monitor"] = monitor;
        }
        let packet = json!(["window-configure", wid, config]);
        self.write_json(packet);
    }

    fn send_window_close(&mut self, wid: u64) {
        let packet = json!(["window-close", wid]);
        self.write_json(packet);
    }

    // Clipboard (plain text, CLIPBOARD selection only - see clipboard.rs / process_hello).

    // Announce that we now own the clipboard and hand the copied text over in the same packet
    // (greedy), so a remote app can paste it without a follow-up request. The server does
    // `str(data)` on the wire value (xpra clipboard/core.py), so the text goes out as a plain JSON
    // string - our JSON-as-YAML writer can't emit raw binary, and doesn't need to here (same
    // reasoning as challenge_client_salt).
    //
    // `clipboard-data` replaced the positional `clipboard-token` packet: selection, then one dict.
    // Unlike the old layout - which could only ever carry a single target's data - `data` maps each
    // target to its own [dtype, dformat, wire_encoding, wire_data] tuple; we still only offer text.
    // `claim` and `greedy` were implicit before (the old packet stopped short of them, so the
    // server defaulted claim=true and kept the greedy flag from our hello); they are now named
    // fields, so we state both (xpra clipboard/core.py _process_clipboard_data).
    fn send_clipboard_data(&mut self, text: &str) {
        let targets = ["UTF8_STRING", "TEXT", "STRING", "text/plain;charset=utf-8", "text/plain"];
        let packet = json!(["clipboard-data", "CLIPBOARD", {
            "claim": true,
            "greedy": true,
            "targets": targets,
            "data": { "UTF8_STRING": ["UTF8_STRING", 8, "bytes", text] },
        }]);
        self.write_json(packet);
    }

    // Ask the server for the current clipboard contents. Only used when it sends us a bare token
    // (no inline data); with a greedy server that path is rarely taken. We keep a single request
    // outstanding, so a constant request_id (echoed back in clipboard-contents) is enough.
    fn send_clipboard_request(&mut self) {
        let packet = json!(["clipboard-request", 0, "CLIPBOARD", "UTF8_STRING"]);
        self.write_json(packet);
    }

    // Reply to the server's clipboard-request with our text. `dtype` is the requested text target
    // echoed back. Note there is no `target` field here (unlike the token) - xpra clipboard/core.py
    // proxy_got_contents: request_id, selection, dtype, dformat, wire_encoding, wire_data.
    fn send_clipboard_contents(&mut self, request_id: u64, dtype: &str, text: &str) {
        let packet = json!(["clipboard-contents", request_id, "CLIPBOARD",
                            dtype, 8, "bytes", text]);
        self.write_json(packet);
    }

    fn send_clipboard_contents_none(&mut self, request_id: u64) {
        let packet = json!(["clipboard-contents-none", request_id, "CLIPBOARD"]);
        self.write_json(packet);
    }

    // Acknowledge a `draw` packet, which is what paces the server's damage output. The legacy
    // `window-draw-ack` packet starts with the packet sequence. The modern, wid-first layout has a
    // distinct name, `window-ack`; reusing `window-draw-ack` for it would make a compatible server
    // interpret the packet sequence as a window id.
    fn send_draw_ack(&mut self, seq: u64, wid: u64, w: u32, h: u32, decode_time: i128, message: String) {
        let packet = draw_ack_packet(
            self.server_backwards_compatible, seq, wid, w, h, decode_time, message,
        );
        self.write_json(packet);
    }

    fn send_ping_echo(&mut self, echotime: u64, sid: String) {
        // fields are echotime, three load averages (we don't report any, hence 0), our last
        // measured client->server latency in ms (or -1 if we've not pinged yet), and the sid. The
        // server stores that latency as its `client_ping_latency` (xpra network_state mixin).
        let packet = json!(["ping_echo", echotime, 0, 0, 0, self.last_client_latency_ms, sid]);
        self.write_json(packet);
    }

    // Send our own `ping` so the server can time the round-trip back to us. The payload is just a
    // monotonic timestamp in ms (matching xpra's `int(1000*monotonic())`); the server echoes it in
    // a `ping_echo` we then match up in process_ping_echo. Fired periodically by start_ping_loop.
    fn send_ping(&mut self) {
        let now_ms = self.start.elapsed().as_millis() as i64;
        let packet = json!(["ping", now_ms]);
        self.write_json(packet);
    }

    // Forward one of our own log records to the server as a `logging-event` packet (the packet
    // formerly known as `logging`). `level` is a python logging level and `message` the formatted
    // text (both prepared by remote_logging.rs); `dtime` is ms since our monotonic start, which the
    // server uses to prefix each line. Only reached once forwarding is switched on in process_hello,
    // so this never fires against a server that would reject it. Mirrors the client->server half of
    // xpra's client/subsystem/logging.py.
    fn send_log(&mut self, level: i64, message: String) {
        let dtime = self.start.elapsed().as_millis() as i64;
        let packet = json!(["logging-event", level, message, dtime]);
        self.write_json(packet);
    }

    fn write_json(&mut self, packet: Value) {
        // once we're on the way out, drop outgoing packets instead of failing on every one:
        // the event loop is winding down but still delivers queued input events.
        if self.exit_code.is_some() {
            return;
        }
        let packet_str = packet.to_string();
        let packet_data = packet_str.as_bytes();
        if let Err(e) = write_packet(&mut self.stream, packet_data) {
            // the server went away mid-write (broken pipe / reset): shut down cleanly rather
            // than panicking. The reader thread may not have noticed yet, so tell the UI thread
            // ourselves - `user_event` is the only place that can reach the `ActiveEventLoop`.
            error!("failed to send packet to the server: {}", e);
            self.exit_code = Some(self.connection_lost_code());
            let _ = self.proxy.send_event(client_packet("connection-lost", &e.to_string()));
        }
    }


    pub fn start_read_loop(&mut self) {
        let proxy = self.proxy.clone();
        let mut stream = self.stream.try_clone().unwrap();
        thread::Builder::new().name("reader".to_string()).spawn(move || loop {
            let t0 = Instant::now();
            let raw = match read_packet(&mut stream) {
                Ok(raw) => raw,
                Err(e) => {
                    // the server closed the connection (or died): hand the reason to the UI
                    // thread, which logs it and exits the event loop.
                    debug!("read loop terminated: {}", e);
                    let _ = proxy.send_event(client_packet("connection-lost", &connection_error(&e)));
                    break;
                }
            };
            let read_elapsed = t0.elapsed();
            // payload + out-of-band chunks: with `chunks` enabled the bulk of a draw packet
            // (the pixel data) arrives as a chunk rather than in the payload.
            let payload_len = raw.size();
            let packet = match parse_packet(raw) {
                Ok(packet) => packet,
                Err(e) => {
                    let _ = proxy.send_event(client_packet("invalid-packet", &e.to_string()));
                    break;
                }
            };
            if packet.get_str(0) == "draw" {
                trace!("perf: draw packet: {:?} bytes read (network) in {:?}", payload_len, read_elapsed);
            }
            if proxy.send_event(packet).is_err() {
                break;
            }
        }).unwrap();
    }


    // Fire a `ping` on a fixed cadence so the server can measure the round-trip to us. Like the
    // reader and decode threads, this posts a synthesized client packet ("send-ping") to the UI
    // thread rather than touching the socket itself - only the UI thread writes to the connection.
    // The thread ends by itself once the event loop is gone (send_event then errors). Started at
    // startup-complete so we never ping before the session is up.
    fn start_ping_loop(&self) {
        let proxy = self.proxy.clone();
        thread::Builder::new().name("ping".to_string()).spawn(move || loop {
            thread::sleep(PING_INTERVAL);
            if proxy.send_event(client_packet("send-ping", "")).is_err() {
                break;
            }
        }).unwrap();
    }

    pub fn start_draw_decode_loop(proxy: EventLoopProxy<Packet>, receiver: Receiver<Packet>,
                                  mmap: Option<Arc<MmapArea>>) {
        thread::Builder::new().name("decode".to_string()).spawn(move || {
            info!("decoding thread started");
            // Per-window H.264 decoders (Windows / Media Foundation). H.264 is inter-frame
            // predicted, so unlike the stateless jpeg/png path each window keeps a persistent,
            // stateful decoder. These COM objects live only on this thread.
            #[cfg(windows)]
            let mut h264_decoders: HashMap<u64, super::mediafoundation::H264Decoder> = HashMap::new();
            loop {
                let mut packet = match receiver.recv() {
                    Ok(packet) => packet,
                    // the UI thread dropped its sender: the client is shutting down.
                    Err(_) => {
                        debug!("decoding thread stopping");
                        break;
                    }
                };
                // window teardown (lost-window) or video stream end (eos) forwarded from the UI
                // thread: release this window's h264 decoder so a following stream restarts from a
                // keyframe (Windows). Both drain the draw queue first (see the dispatch side).
                let ptype = packet.get_str(0);
                if ptype == "lost-window" || ptype == "eos" {
                    #[cfg(windows)]
                    {
                        let key = packet.get_u64(1);
                        if h264_decoders.remove(&key).is_some() {
                            debug!("released h264 decoder for {:?} on window {:#x}", ptype, key);
                        }
                    }
                    continue;
                }
                let wid = packet.get_i64(1);
                let w = packet.get_i32(4);
                let h = packet.get_i32(5);
                let coding = packet.get_str(6);
                // an mmap draw has no pixel data in the packet at all - field 7 holds a copy of
                // its chunk list, which read_mmap_draw picks up from the options instead.
                let data = if coding == "mmap" { Vec::new() } else { packet.get_bytes(7) };
                let seq = packet.get_i64(8);
                debug!("wid {:#x} got {:?}x{:?} {:?} draw packet", wid, w, h, coding);

                let mut main = packet.main.to_vec();
                let mut raw = HashMap::new();
                let t0 = Instant::now();
                // Ok(Some(pixels)) = a frame is ready; Ok(None) = input consumed but no frame yet
                // (decoder warm-up) -- we must still ack the sequence; Err = decode failure.
                let result: Result<Option<Vec<u8>>, String> = if coding == "h264" {
                    #[cfg(windows)]
                    {
                        let key = wid as u64;
                        // colour range signalled per-stream by the encoder; absent in steady state
                        // (xpra omits it once settled), so None means "unchanged" to the decoder.
                        let full_range = packet.get_hash_bool(10, "full-range".to_string());
                        let ensured = if h264_decoders.contains_key(&key) {
                            Ok(())
                        } else {
                            super::mediafoundation::H264Decoder::new()
                                .map(|d| { h264_decoders.insert(key, d); })
                        };
                        ensured.and_then(|()| {
                            h264_decoders.get_mut(&key).unwrap()
                                .decode(&data, w.max(0) as u32, h.max(0) as u32, full_range)
                        })
                    }
                    #[cfg(not(windows))]
                    {
                        Err("h264 decoding is only supported on Windows".to_string())
                    }
                } else if coding == "mmap" {
                    match &mmap {
                        Some(area) => read_mmap_draw(&packet, area).map(Some),
                        // the server only sends these once it has verified our area, so this
                        // cannot happen - but it must not be painted as if it were pixel data.
                        None => Err("received an mmap draw without an mmap area".to_string()),
                    }
                } else {
                    draw_decoder::decode(&coding, data).map(Some)
                };
                let decode_elapsed = t0.elapsed();
                trace!("perf: draw packet: {:?}x{:?} {:?} decoded in {:?}", w, h, coding, decode_elapsed);
                let mut decode_time_us = None;
                match result {
                    Err(message) => {
                        error!("draw decoding error for {:?} sequence {:?}: {:?}", coding, seq, message);
                        main[0] = Yaml::String("decoding-failed".to_string());
                        main[7] = Yaml::String(message.to_string());
                    }
                    Ok(pixels) => {
                        // an empty payload (None) means "no frame this time": the UI thread will
                        // ack the sequence without painting.
                        raw.insert(7, pixels.unwrap_or_default());
                        main[0] = Yaml::String("draw-decoded".to_string());
                        decode_time_us = Some(decode_elapsed.as_micros() as i64);
                    }
                }
                let patched_packet = Packet { main, raw, decode_time_us };
                if proxy.send_event(patched_packet).is_err() {
                    break;
                }
            }
        }).unwrap();
    }


    fn do_process_packet(&mut self, event_loop: &ActiveEventLoop, packet_type: &str, packet: Packet) {
        let mut p = packet;
        match packet_type {
            "hello" => {
                assert!(p.len() > 1);
                self.process_hello(event_loop, &p.main[1]);
            }
            #[cfg(windows)]
            AUDIO_CAPABILITIES_PACKET => {
                if p.len() > 1 {
                    self.process_audio_capabilities(&p.main[1]);
                } else {
                    warn!("ignoring malformed audio-capabilities packet");
                }
            }
            // `sound-data` is the incoming compatibility alias only. All packets we emit use the
            // canonical `audio-*` names.
            #[cfg(windows)]
            packet_type if audio::is_audio_data_type(packet_type) => self.process_audio_data(&mut p),
            #[cfg(windows)]
            "audio-latency" => self.report_audio_latency(p.get_u32(1)),
            #[cfg(windows)]
            "audio-worker-failed" => self.disable_audio(&p.get_str(1)),
            "encodings" => debug!("got server encodings: {:?}", p.main[1]),
            "startup-complete" => {
                info!("startup complete!");
                // the session is up: start pinging the server so it can track our latency.
                if !self.startup_complete {
                    self.startup_complete = true;
                    self.start_ping_loop();
                }
            }
            "new-window" => self.process_new_common(event_loop, &p, false),
            "new-override-redirect" => self.process_new_common(event_loop, &p, true),
            "window-move-resize" => self.process_window_move_resize(&p),
            "configure-override-redirect" => self.process_window_move_resize(&p),
            "initiate-moveresize" => self.process_initiate_moveresize(&p),
            "raise-window" => self.process_raise_window(&p),
            "show-desktop" => self.process_show_desktop(&p),
            "pointer-position" => self.process_pointer_position(&p),
            "pointer-grab" => self.process_pointer_grab(&p),
            "pointer-ungrab" => self.process_pointer_ungrab(&p),
            "lost-window" => {
                self.process_lost_window(&p);
                // forward to the decode thread so it can drop this window's persistent h264
                // decoder; routed through the same channel as draws, so any still-queued draws
                // for this window drain before the decoder is released.
                #[cfg(windows)]
                { let _ = self.decode_sender.send(p); }
            }
            "eos" => {
                // video stream ended: forward to the decode thread to drop this window's h264
                // decoder, over the draw channel so any queued draws for the old stream drain first.
                #[cfg(windows)]
                { let _ = self.decode_sender.send(p); }
            }
            // ["setting-change", setting, value]: server-pushed session settings we don't act on
            // (xpra's own client no-ops most of these); log rather than warn about "unhandled".
            "setting-change" => debug!("ignoring setting-change: {:?}", p.get_str(1)),
            "bell" => self.process_bell(&p),
            "cursor" => self.process_cursor(event_loop, &mut p),
            "notify_show" => self.process_notify_show(&p),
            "notify_close" => self.process_notify_close(&p),
            "window-icon" => self.process_window_icon(&mut p),
            "window-metadata" => self.process_window_metadata(&p),
            "server-event" => self.process_server_event(&p),
            "draw" => {
                if self.decode_sender.send(p).is_err() {
                    error!("cannot decode: the decoding thread has stopped");
                }
            }
            "draw-decoded" => self.process_draw_decoded(&mut p),
            // both are client-side packet types the decode thread synthesizes for a draw it could
            // not turn into pixels; either way the sequence still has to be acked, or the server's
            // damage bookkeeping for that window stops making progress.
            "draw-failed" | "decoding-failed" => self.process_draw_failed(&p),
            "ping" => self.process_ping(&p),
            // our own periodic ping, fired by the ping timer thread (start_ping_loop); "send-ping"
            // is a client-side packet type like "draw-decoded", not something on the wire.
            "send-ping" => self.send_ping(),
            // one of our own log records, handed here by the remote logger (remote_logging.rs) to
            // be turned into a wire `logging` packet; "send-log" is client-side only, like above.
            "send-log" => self.send_log(p.get_i64(1), p.get_str(2)),
            // the server's echo of one of our pings: measures the client->server round-trip.
            "ping_echo" => self.process_ping_echo(&p),
            "challenge" => self.process_challenge(event_loop, &mut p),
            // ["challenge-password", pw] / ["challenge-cancel"]: synthesized locally by the
            // pinentry worker thread (see prompt_password_pinentry), delivered on the UI thread so
            // it can compute the response and re-send hello / quit - a challenge equivalent of the
            // decode thread's "draw-decoded".
            "challenge-password" => {
                let password = p.get_str(1);
                self.answer_challenge(&password);
            }
            "challenge-cancel" => self.cancel_auth(event_loop),
            // pinentry could not run (e.g. no display); fall back to the built-in dialog. The
            // prompt text rides along in field 1.
            "challenge-fallback-dialog" => {
                let prompt_text = p.get_str(1);
                self.show_auth_dialog(event_loop, prompt_text);
            }
            // clipboard (plain text). The server toggles syncing, takes/gives ownership via
            // token/data, and pulls/pushes contents; see the process_clipboard_* handlers below and
            // clipboard.rs. "clipboard-changed" is our own synthesized type, posted by the clipboard
            // thread when the local OS clipboard changed - the analogue of "send-ping"/"draw-decoded".
            "set-clipboard-enabled" => {
                self.clipboard_enabled = p.get_bool(1);
                debug!("clipboard sync {}", if self.clipboard_enabled { "enabled" } else { "disabled" });
            }
            "clipboard-token" => self.process_clipboard_token(&mut p),
            "clipboard-request" => self.process_clipboard_request(&p),
            "clipboard-contents" => self.process_clipboard_contents(&mut p),
            "clipboard-contents-none" => debug!("clipboard-contents-none"),
            "clipboard-pending-requests" => {} // server-side request count; nothing to render
            "clipboard-changed" => self.process_clipboard_changed(&p),
            // ["tray-exit"]: the "Exit" item of the Windows system tray menu (see tray.rs). A
            // client-side packet type like "send-ping": the tray's window procedure runs on the UI
            // thread but has no `ActiveEventLoop`, so it posts this and the quit happens here.
            "tray-exit" => {
                info!("exit requested from the system tray");
                // say goodbye the way xpra's own client does (`disconnect` is now `connection-close`).
                // This has to come before quit(), which sets exit_code and turns write_json into a
                // no-op.
                self.write_json(json!(["connection-close", "client exit"]));
                self.quit(event_loop, ExitCode::Ok);
            }
            "disconnect" => self.process_disconnect(event_loop, &p),
            "connection-lost" => {
                // synthesized locally (see `client_packet`): the write path has already logged
                // the error that got it here, so only log if this is the first we hear of it.
                if self.exit_code.is_none() {
                    warn!("connection lost: {}", p.get_str(1));
                }
                let exit_code = self.connection_lost_code();
                self.quit(event_loop, exit_code);
            }
            "invalid-packet" => {
                error!("invalid packet received: {}", p.get_str(1));
                // garbage on a connection that never became a session usually means we're not
                // talking to an xpra server at all, so report that rather than a packet failure:
                let exit_code = if self.startup_complete {
                    ExitCode::PacketFailure
                } else {
                    ExitCode::ConnectionFailed
                };
                self.quit(event_loop, exit_code);
            }
            other => warn!("unhandled packet type {:?}", other),
        }
    }

    // ["server-event", event_type, *args]: informational server lifecycle events. Advertising
    // `events: true` in hello enables these; they complement rather than replace the dedicated
    // protocol packets, so log them without changing client state.
    fn process_server_event(&self, packet: &Packet) {
        if packet.len() < 2 {
            warn!("ignoring malformed server-event packet with no event type");
            return;
        }
        let event_type = packet.get_str(1);
        if event_type.is_empty() {
            warn!("ignoring malformed server-event packet with an invalid event type");
            return;
        }
        info!("server event: {}", event_type);
        if packet.len() > 2 {
            debug!("server event {:?} arguments: {:?}", event_type, &packet.main[2..]);
        }
    }

    // ["disconnect", reason, *info] - see xpra's `server_disconnect_exit_code` in
    // `client/base/client.py`: most disconnects are the server saying goodbye (exit code `OK`);
    // the exceptions are authentication failures and anything whose reason reads as an error.
    fn process_disconnect(&mut self, event_loop: &ActiveEventLoop, packet: &Packet) {
        let info: Vec<String> = (1..packet.len()).map(|i| packet.get_str(i as u8)).collect();
        let reason = info.first().cloned().unwrap_or_default();
        let message = info.join(", ");
        let exit_code = if info.iter().any(|i| i == "authentication failed") {
            ExitCode::AuthenticationFailed
        } else if disconnect_is_an_error(&reason) {
            error!("server connection failure: {}", message);
            // being kicked out before the session is up is really a failure to connect:
            if self.startup_complete { ExitCode::Failure } else { ExitCode::ConnectionFailed }
        } else {
            info!("disconnected by the server: {}", message);
            ExitCode::Ok
        };
        self.quit(event_loop, exit_code);
    }

    // ["challenge", server_salt, cipher, digest, salt_digest, prompt]: the server wants a password.
    // We only implement the hmac+sha256 password digest (see send_hello / net::sha256); the reply
    // is a second hello carrying the challenge response. Mirrors xpra's client/base/challenge.py.
    fn process_challenge(&mut self, event_loop: &ActiveEventLoop, packet: &mut Packet) {
        if self.pending_challenge.is_some() || self.auth_dialog.is_some() {
            // we answer a single challenge; a repeat means our answer was rejected, and the server
            // will also send a disconnect ("authentication failed") that ends the session.
            warn!("ignoring repeated challenge");
            return;
        }
        let server_salt = packet.get_bytes(1);
        let digest = packet.get_str(3);
        let salt_digest = if packet.len() >= 5 { packet.get_str(4) } else { "xor".to_string() };
        let prompt = if packet.len() >= 6 { packet.get_str(5) } else { "password".to_string() };
        if server_salt.is_empty() {
            error!("authentication challenge has no server salt");
            self.quit(event_loop, ExitCode::AuthenticationFailed);
            return;
        }
        // we advertised only hmac+sha256 for both digests, so that is all the server should pick.
        // Anything else (including the xor/des digests, which xpra only allows over an encrypted
        // link we don't have) we cannot answer - fail cleanly rather than hang until the server's
        // authentication timeout.
        if digest != "hmac+sha256" || salt_digest != "hmac+sha256" {
            error!("server requested an unsupported challenge digest ({digest:?}/{salt_digest:?})");
            self.quit(event_loop, ExitCode::AuthenticationFailed);
            return;
        }
        self.pending_challenge = Some(server_salt);

        // password source 1: the connection dialog, when the user filled its (optional) password
        // field - they have already been asked, so do not ask again.
        if let Some(pw) = self.password.clone().filter(|pw| !pw.is_empty()) {
            info!("authenticating with the password from the connection dialog");
            self.answer_challenge(&pw);
            return;
        }
        // source 2: XPRA_PASSWORD, for non-interactive (scripted / tested) runs.
        if let Ok(pw) = env::var("XPRA_PASSWORD") {
            if !pw.is_empty() {
                info!("authenticating with the password from XPRA_PASSWORD");
                self.answer_challenge(&pw);
                return;
            }
        }
        // the server's prompt is usually descriptive already (e.g. "password for user 'foo'"),
        // so just prefix it, mirroring xpra's own "Please enter the {prompt}".
        let prompt_text = format!("Enter {prompt}");
        // source 3: pinentry when it is on PATH - a native secure prompt. It blocks while the user
        // types, so it runs on a worker thread that posts the result back (see spawn_pinentry).
        if let Some(prog) = find_pinentry() {
            debug!("prompting for the password via {prog}");
            spawn_pinentry(prog, prompt_text, self.proxy.clone());
            return;
        }
        // source 4: the built-in dialog - the universal fallback (e.g. Windows without GnuPG).
        self.show_auth_dialog(event_loop, prompt_text);
    }

    // compute the challenge response for `password` and send it as a second hello. `client_salt` is
    // a random *ASCII* hex string (not raw bytes) so it survives our JSON-as-YAML writer unchanged;
    // the server utf-8-encodes it back to the same bytes, so the digests still match (verified).
    fn answer_challenge(&mut self, password: &str) {
        let server_salt = match self.pending_challenge.take() {
            Some(salt) => salt,
            None => {
                warn!("a password arrived but no challenge is pending");
                return;
            }
        };
        let client_salt = secure_hex(32);
        let salt = hmac_sha256_hex(client_salt.as_bytes(), &server_salt);
        let response = hmac_sha256_hex(password.as_bytes(), salt.as_bytes());
        debug!("answering authentication challenge");
        self.send_hello(Some((response, client_salt)));
    }

    fn show_auth_dialog(&mut self, event_loop: &ActiveEventLoop, prompt_text: String) {
        let context = match self.softbuffer_ctx.as_ref() {
            Some(context) => context,
            None => {
                error!("cannot show the password dialog: no softbuffer context");
                self.quit(event_loop, ExitCode::AuthenticationFailed);
                return;
            }
        };
        match AuthDialog::new(event_loop, context, prompt_text) {
            Ok(dialog) => self.auth_dialog = Some(dialog),
            Err(e) => {
                error!("cannot show the password dialog: {e}");
                self.quit(event_loop, ExitCode::AuthenticationFailed);
            }
        }
    }

    fn handle_auth_dialog_event(&mut self, event_loop: &ActiveEventLoop, event: WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => {
                if let Some(dialog) = self.auth_dialog.as_mut() {
                    dialog.draw();
                }
            }
            WindowEvent::CloseRequested => self.cancel_auth(event_loop),
            // as in the connection dialog: the presses winit synthesizes for keys already held
            // when the window takes focus are not typed input, and would end up in the password.
            WindowEvent::KeyboardInput { is_synthetic: true, .. } => {}
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                let action = match self.auth_dialog.as_mut() {
                    Some(dialog) => dialog.handle_key(&key_event),
                    None => return,
                };
                match action {
                    DialogAction::None => {}
                    DialogAction::Submit => {
                        let password = self
                            .auth_dialog
                            .take()
                            .map(|dialog| dialog.into_password())
                            .unwrap_or_default();
                        self.answer_challenge(&password);
                    }
                    DialogAction::Cancel => self.cancel_auth(event_loop),
                }
            }
            _ => {}
        }
    }

    fn cancel_auth(&mut self, event_loop: &ActiveEventLoop) {
        error!("authentication cancelled");
        self.auth_dialog = None;
        self.pending_challenge = None;
        self.quit(event_loop, ExitCode::AuthenticationFailed);
    }

    fn process_hello(&mut self, event_loop: &ActiveEventLoop, hello: &Yaml) {
        self.process_mmap_caps(event_loop, hello);
        match &hello {
            Yaml::Hash(hash) => {
                let version_key: Yaml = Yaml::String(VERSION_KEY_STR.to_string());
                let version = &hash[&version_key];
                if let Yaml::String(version_str) = version {
                    info!("server version {:?}", version_str);
                    self.server_version = version_str.to_string();
                }
                // `damage-sequence` is registered only when the server runs in backwards-
                // compatible mode. `window-ack` confirms this is a new enough server for the
                // modern acknowledgement; if the capability is absent or predates that packet,
                // retain the safe default above.
                if let Some(backwards_compatible) = server_backwards_compatible(hello) {
                    self.server_backwards_compatible = backwards_compatible;
                    debug!(
                        "server backwards-compatible packet mode: {}",
                        self.server_backwards_compatible,
                    );
                }
                // The server advertises whether it accepts forwarded client logs as
                // `remote-logging: {receive, send}` (xpra server/subsystem/logging.py). When it
                // receives, drop our proxy into the shared sink so the global logger starts
                // forwarding info-and-above records to the server (see remote_logging.rs); we skip
                // it otherwise, so we never send `logging` packets a server would reject.
                let receives_logs = hash.get(&Yaml::String("remote-logging".to_string()))
                    .and_then(|rl| yaml_hash_bool(rl, "receive".to_string()))
                    .unwrap_or(false);
                if receives_logs {
                    *self.log_sink.lock().unwrap() = Some(self.proxy.clone());
                    info!("remote logging enabled");
                }
                // The server advertises clipboard support as a non-empty `clipboard` dict (xpra
                // server/subsystem/clipboard.py get_caps; absent when started with --clipboard=no).
                // When present, start the clipboard thread and enable syncing; otherwise we stay
                // inert and never send clipboard packets - like the remote-logging gate above.
                let server_clipboard = hash.get(&Yaml::String("clipboard".to_string()))
                    .map(|c| matches!(c, Yaml::Hash(_)))
                    .unwrap_or(false);
                if server_clipboard && self.clipboard.is_none() {
                    let (tx, rx) = channel::<String>();
                    start_clipboard_loop(self.proxy.clone(), rx);
                    self.clipboard = Some(tx);
                    self.clipboard_enabled = true;
                    info!("clipboard sync enabled");
                }
                #[cfg(windows)]
                if self.audio_worker.is_some()
                    && !self.audio_protocol.capabilities_sent
                    && audio::async_requested(hello)
                {
                    self.audio_protocol.server_av_sync = audio::server_av_sync_enabled(hello);
                    self.write_json(json!([
                        AUDIO_CAPABILITIES_PACKET,
                        audio::receive_capabilities(),
                    ]));
                    self.audio_protocol.capabilities_sent = true;
                    debug!("sent asynchronous Opus receive capabilities");
                }
            },
            _ => error!("unexpected hello data type: {:?}", hello),
        }
    }

    // Second half of the mmap handshake: the server has opened the area we offered in our hello,
    // written its own token into it and told us where. Verify it, then drop the backing file -
    // the server has it mapped by now and neither side needs the directory entry any more.
    // Keeping the area means the server will send us `mmap` draws; dropping it means it won't.
    fn process_mmap_caps(&mut self, event_loop: &ActiveEventLoop, hello: &Yaml) {
        let area = match &self.mmap {
            Some(area) => area,
            None => return,
        };
        let verified = area.check_server_caps(hello);
        let size = area.size();
        // whatever the outcome: the server has the file mapped by now, so the directory entry has
        // done its job. The mapping itself survives being unlinked.
        area.unlink();
        match verified {
            Ok(true) => {
                info!("enabled fast mmap picture transfers using a {}MB shared memory area",
                      size / 1024 / 1024);
            }
            Ok(false) => {
                debug!("the server is not using our mmap area");
                self.mmap = None;
            }
            Err(message) => {
                // The server believes mmap is live and will send draws referencing an area we
                // cannot trust - most likely a file of the same name on a *different* host.
                error!("mmap {}", message);
                self.mmap = None;
                self.quit(event_loop, ExitCode::MmapTokenFailure);
            }
        }
    }

    // Windows speaker forwarding -------------------------------------------------------------

    #[cfg(windows)]
    fn process_audio_capabilities(&mut self, capabilities: &Yaml) {
        if self.audio_worker.is_none()
            || !self.audio_protocol.capabilities_sent
            || self.audio_protocol.negotiated
        {
            return;
        }
        if !audio::server_can_send_opus(capabilities) {
            debug!("server cannot send bare Opus audio");
            return;
        }
        self.audio_protocol.negotiated = true;
        self.send_audio_control("start", json!(CODEC));
        self.audio_sync_reporter = LatencyReporter::default();
        self.report_audio_latency(0);
        info!("Opus speaker forwarding negotiated");
    }

    #[cfg(windows)]
    fn process_audio_data(&mut self, packet: &mut Packet) {
        if !self.audio_protocol.negotiated || self.audio_worker.is_none() {
            return;
        }
        let incoming = match IncomingAudio::parse(packet) {
            Ok(incoming) => incoming,
            Err(error) => {
                warn!("ignoring malformed audio-data packet: {error}");
                return;
            }
        };
        if !self.audio_protocol.accepts_sequence(incoming.metadata.sequence) {
            debug!(
                "ignoring audio data for old sequence {:?} (current is {})",
                incoming.metadata.sequence,
                self.audio_protocol.sequence,
            );
            return;
        }

        if incoming.metadata.start_of_stream {
            let stream_codec = incoming.metadata.codec.as_deref()
                .filter(|codec| !codec.is_empty())
                .unwrap_or(&incoming.codec);
            match self.audio_protocol.begin(stream_codec, incoming.metadata.sequence) {
                Ok(sequence) => {
                    let result = self.audio_worker.as_ref()
                        .map(|worker| worker.reset(sequence))
                        .unwrap_or(Err(EnqueueError::Stopped));
                    if !self.handle_audio_enqueue(result, "reset", true) {
                        return;
                    }
                    self.audio_sync_reporter = LatencyReporter::default();
                    self.report_audio_latency(0);
                    debug!("starting Opus audio sequence {sequence}");
                }
                Err(error) => {
                    warn!("rejecting audio stream: {error}");
                    self.stop_audio_stream(true);
                    return;
                }
            }
        } else if !self.audio_protocol.active {
            debug!("dropping audio data outside an active stream");
            return;
        }

        if incoming.metadata.end_of_stream {
            let sequence = self.audio_protocol.sequence;
            let result = self.audio_worker.as_ref()
                .map(|worker| worker.end(sequence))
                .unwrap_or(Err(EnqueueError::Stopped));
            if !self.handle_audio_enqueue(result, "end-of-stream", true) {
                return;
            }
            let new_sequence = self.audio_protocol.finish();
            self.send_audio_control("new-sequence", json!(new_sequence));
            self.report_audio_latency(0);
            debug!("audio sequence {sequence} ended");
            return;
        }

        if incoming.codec != CODEC {
            warn!(
                "audio codec changed from {CODEC:?} to {:?}; stopping the stream",
                incoming.codec,
            );
            self.stop_audio_stream(true);
            return;
        }

        // Bundled stream headers must be submitted before this packet's main payload.
        for header in incoming.headers {
            if !self.process_opus_buffer(header, None, None) {
                return;
            }
        }
        if !incoming.data.is_empty() {
            self.process_opus_buffer(
                incoming.data,
                incoming.metadata.timestamp_ns,
                incoming.metadata.duration_ns,
            );
        }
    }

    #[cfg(windows)]
    fn process_opus_buffer(
        &mut self,
        data: Vec<u8>,
        timestamp_ns: Option<i64>,
        duration_ns: Option<i64>,
    ) -> bool {
        if data.starts_with(b"OpusHead") {
            let header = match OpusHeader::parse(&data) {
                Ok(header) => header,
                Err(error) => {
                    warn!("invalid Opus stream header: {error}");
                    self.stop_audio_stream(true);
                    return false;
                }
            };
            let sequence = self.audio_protocol.sequence;
            let result = self.audio_worker.as_ref()
                .map(|worker| worker.configure(sequence, header, data))
                .unwrap_or(Err(EnqueueError::Stopped));
            if self.handle_audio_enqueue(result, "OpusHead", true) {
                self.audio_protocol.header_seen = true;
                return true;
            }
            return false;
        }
        if audio::is_opus_tags(&data) {
            return true;
        }
        if !self.audio_protocol.header_seen {
            debug!("dropping Opus payload received before OpusHead");
            return true;
        }
        let sequence = self.audio_protocol.sequence;
        let result = self.audio_worker.as_ref()
            .map(|worker| worker.packet(
                sequence,
                data,
                timestamp_ns,
                duration_ns,
                self.start.elapsed().as_millis() as u64,
            ))
            .unwrap_or(Err(EnqueueError::Stopped));
        self.handle_audio_enqueue(result, "packet", false)
    }

    #[cfg(windows)]
    fn handle_audio_enqueue(
        &mut self,
        result: Result<(), EnqueueError>,
        operation: &str,
        critical: bool,
    ) -> bool {
        match result {
            Ok(()) => true,
            Err(EnqueueError::Full) => {
                if critical {
                    self.disable_audio(&format!(
                        "audio worker queue was full while queuing {operation}",
                    ));
                    return false;
                }
                if !self.audio_queue_warned {
                    warn!("audio worker queue is full; dropping audio packets until it catches up");
                    self.audio_queue_warned = true;
                }
                false
            }
            Err(EnqueueError::Stopped) => {
                self.disable_audio(&format!("audio worker stopped while queuing {operation}"));
                false
            }
        }
    }

    #[cfg(windows)]
    fn send_audio_control(&mut self, command: &str, argument: Value) {
        self.write_json(audio::control_packet(command, argument));
    }

    #[cfg(windows)]
    fn report_audio_latency(&mut self, total_ms: u32) {
        if !self.audio_protocol.negotiated || !self.audio_protocol.server_av_sync {
            return;
        }
        if let Some(total_ms) = self.audio_sync_reporter.update(total_ms) {
            self.send_audio_control("sync", json!(total_ms));
        }
    }

    #[cfg(windows)]
    fn stop_audio_stream(&mut self, tell_server: bool) {
        let sequence = self.audio_protocol.sequence;
        let result = self.audio_worker.as_ref()
            .map(|worker| worker.end(sequence))
            .unwrap_or(Err(EnqueueError::Stopped));
        if !self.handle_audio_enqueue(result, "stop", true) {
            return;
        }
        if tell_server {
            self.send_audio_control("stop", json!(sequence));
        }
        let new_sequence = self.audio_protocol.finish();
        self.send_audio_control("new-sequence", json!(new_sequence));
        self.audio_sync_reporter = LatencyReporter::default();
        self.report_audio_latency(0);
    }

    #[cfg(windows)]
    fn disable_audio(&mut self, error: &str) {
        if self.audio_worker.is_none() {
            return;
        }
        warn!("speaker forwarding disabled for this session: {error}");
        // Recovery failures must explicitly stop the server source even if no SOS arrived yet.
        let sequence = self.audio_protocol.sequence;
        self.send_audio_control("stop", json!(sequence));
        let new_sequence = self.audio_protocol.finish();
        self.send_audio_control("new-sequence", json!(new_sequence));
        self.audio_protocol.negotiated = false;
        self.audio_worker = None;
    }

    // Clipboard handlers (plain text). See send_clipboard_* for the outbound side and clipboard.rs
    // for the OS-clipboard thread. All are no-ops unless syncing is on.

    // The remote end took ownership of the clipboard. A greedy server puts the copied text right in
    // the token (fields 3..8 of the legacy layout: target, dtype, dformat, wire_encoding,
    // wire_data), so we write it straight to the local clipboard. A bare token carries no data - we
    // pull it with a clipboard-request instead.
    fn process_clipboard_token(&mut self, packet: &mut Packet) {
        if !self.clipboard_enabled {
            return;
        }
        if packet.len() >= 8 {
            if let Some(text) = self.clipboard_text(packet, 6, 7) {
                self.set_local_clipboard(text);
            }
        } else {
            self.send_clipboard_request();
        }
    }

    // The server asks for our clipboard contents (a remote app is pasting). Reply with the latest
    // local text, which the clipboard thread's poll keeps in `last_clipboard`. We only serve plain
    // text, so a request for anything else (a TARGETS enumeration, an image, ...) gets "none" - and
    // we echo the requested text target back as the reply's dtype.
    fn process_clipboard_request(&mut self, packet: &Packet) {
        let request_id = packet.get_u64(1);
        let target = packet.get_str(3);
        let is_text = matches!(target.as_str(),
            "UTF8_STRING" | "TEXT" | "STRING" | "text/plain;charset=utf-8" | "text/plain");
        if !self.clipboard_enabled || !is_text || self.last_clipboard.is_empty() {
            self.send_clipboard_contents_none(request_id);
            return;
        }
        let text = self.last_clipboard.clone();
        self.send_clipboard_contents(request_id, &target, &text);
    }

    // The server's reply to a clipboard-request we made for a bare token: the pulled text. Layout
    // has no target field - request_id, selection, dtype, dformat, wire_encoding, wire_data.
    fn process_clipboard_contents(&mut self, packet: &mut Packet) {
        if !self.clipboard_enabled {
            return;
        }
        if packet.len() >= 7 {
            if let Some(text) = self.clipboard_text(packet, 5, 6) {
                self.set_local_clipboard(text);
            }
        }
    }

    // The clipboard thread saw the local clipboard change (a local copy): claim the clipboard on
    // the remote side by sending a token carrying the new text. The `last_clipboard` guard drops a
    // value we ourselves just wrote from a remote paste, so it doesn't bounce back to the server.
    fn process_clipboard_changed(&mut self, packet: &Packet) {
        if !self.clipboard_enabled {
            return;
        }
        let text = packet.get_str(1);
        if text.is_empty() || text == self.last_clipboard {
            return;
        }
        self.last_clipboard = text.clone();
        self.send_clipboard_data(&text);
    }

    // Decode a plain-text clipboard payload. xpra sends 8-bit text with wire encoding "bytes" (the
    // only text encoding - clipboard/core.py); the bytes ride as a YAML !!binary scalar, which
    // get_bytes base64-decodes. Non-text encodings ("integers"/"atoms") aren't text - skip them.
    fn clipboard_text(&self, packet: &mut Packet, enc_index: u8, data_index: u8) -> Option<String> {
        let encoding = packet.get_str(enc_index);
        if encoding != "bytes" {
            debug!("ignoring clipboard data with wire encoding {:?}", encoding);
            return None;
        }
        let bytes = packet.get_bytes(data_index);
        if bytes.is_empty() {
            return None;
        }
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    // Put text on the local OS clipboard (via the clipboard thread) and remember it, so the
    // thread's poll doesn't report our own write back as a local change (which would loop it
    // straight back to the server).
    fn set_local_clipboard(&mut self, text: String) {
        self.last_clipboard = text.clone();
        if let Some(tx) = &self.clipboard {
            let _ = tx.send(text);
        }
    }

    fn process_new_common(&mut self, event_loop: &ActiveEventLoop, packet: &Packet, override_redirect: bool) {
        let wid = packet.get_u64(1);
        debug!("new-window {:#x}, override-redirect={:?}", wid, override_redirect);
        let x = packet.get_i32(2);
        let y = packet.get_i32(3);
        let w = packet.get_u32(4);
        let h = packet.get_u32(5);
        let metadata = WindowMetadataUpdate::parse(&packet.main[6]);
        let title = metadata.title.clone().unwrap_or_default();
        // override-redirect windows are never decorated; otherwise honour the metadata flag
        // (absent means decorated, as in xpra's own client - see `client/gui/window_base.py`)
        let decorated = !override_redirect
            && metadata.decorations.unwrap_or(true);

        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes()
            .with_title(&title)
            .with_position(PhysicalPosition::new(x, y))
            .with_inner_size(PhysicalSize::new(w.max(1), h.max(1)))
            .with_decorations(decorated)
            .with_resizable(!override_redirect);
        #[cfg(target_os = "linux")]
        {
            use winit::platform::x11::WindowAttributesExtX11;
            attrs = attrs.with_override_redirect(override_redirect);
        }

        let window = match event_loop.create_window(attrs) {
            Ok(window) => Rc::new(window),
            Err(e) => {
                error!("failed to create window: {:?}", e);
                return;
            }
        };
        info!("new-window {:#x} : {:?}", wid, title);

        // start the window off with the current session cursor (see process_cursor):
        if let Some(cursor) = self.current_cursor.clone() {
            window.set_cursor(cursor);
        }

        let context = self.softbuffer_ctx.as_ref().expect("softbuffer context not initialized");
        let mut xpra_window = XpraWindow::new(wid, window.clone(), context, w, h, override_redirect);
        Self::apply_window_metadata(&mut xpra_window, metadata);
        xpra_window.mapped = true;
        self.id_map.insert(window.id(), wid);
        self.windows.insert(wid, xpra_window);

        if !override_redirect {
            self.send_window_map(wid, x, y, w, h);
        }
    }

    fn process_window_move_resize(&mut self, packet: &Packet) {
        let wid = packet.get_u64(1);
        let window = match self.windows.get_mut(&wid) {
            Some(window) => window,
            None => {
                error!("cannot move-resize: window {:#x} not found", wid);
                return;
            }
        };
        let x = packet.get_i32(2);
        let y = packet.get_i32(3);
        let w = packet.get_u32(4);
        let h = packet.get_u32(5);

        if let Some(outer) = window.to_outer_position(x, y) {
            window.window.set_outer_position(outer);
        } else {
            debug!("window {:#x}: absolute positioning is not supported on this platform (Wayland)", wid);
        }
        let _ = window.window.request_inner_size(PhysicalSize::new(w.max(1), h.max(1)));
    }

    // ["initiate-moveresize", wid, x_root, y_root, direction, button, source_indication]
    // The server forwards a window's _NET_WM_MOVERESIZE request (an app calling the EWMH hint,
    // e.g. dragging its own client-side titlebar) so we can start an interactive move/resize
    // through our own window manager. winit's drag_window()/drag_resize_window() map straight
    // onto the same primitive (X11 _NET_WM_MOVERESIZE, Wayland xdg_toplevel move/resize) - and
    // interactive drag is in fact the *one* way to reposition a window on Wayland, where the
    // absolute positioning used by window-move-resize isn't available to clients.
    // `direction` reuses the _NET_WM_MOVERESIZE integer constants; the keyboard-initiated ones
    // (9/10) and cancel (11) have no winit equivalent and are ignored. These only take effect
    // while the initiating pointer button is still held (the WM adopts the pointer grab), so a
    // request whose grab has already been released gets silently dropped by the WM.
    fn process_initiate_moveresize(&mut self, packet: &Packet) {
        let wid = packet.get_u64(1);
        let direction = packet.get_u32(4);
        let window = match self.windows.get(&wid) {
            Some(window) => window,
            None => {
                error!("cannot initiate move-resize: window {:#x} not found", wid);
                return;
            }
        };
        // None = a plain move (direction 8, _NET_WM_MOVERESIZE_MOVE); the rest are resize edges.
        let resize = match direction {
            0 => Some(ResizeDirection::NorthWest), // _NET_WM_MOVERESIZE_SIZE_TOPLEFT
            1 => Some(ResizeDirection::North),     // _NET_WM_MOVERESIZE_SIZE_TOP
            2 => Some(ResizeDirection::NorthEast), // _NET_WM_MOVERESIZE_SIZE_TOPRIGHT
            3 => Some(ResizeDirection::East),      // _NET_WM_MOVERESIZE_SIZE_RIGHT
            4 => Some(ResizeDirection::SouthEast), // _NET_WM_MOVERESIZE_SIZE_BOTTOMRIGHT
            5 => Some(ResizeDirection::South),     // _NET_WM_MOVERESIZE_SIZE_BOTTOM
            6 => Some(ResizeDirection::SouthWest), // _NET_WM_MOVERESIZE_SIZE_BOTTOMLEFT
            7 => Some(ResizeDirection::West),      // _NET_WM_MOVERESIZE_SIZE_LEFT
            8 => None,                             // _NET_WM_MOVERESIZE_MOVE
            _ => {
                debug!("ignoring unsupported initiate-moveresize direction {:?}", direction);
                return;
            }
        };
        let result = match resize {
            Some(dir) => window.window.drag_resize_window(dir),
            None => window.window.drag_window(),
        };
        if let Err(e) = result {
            debug!("initiate-moveresize for window {:#x} was not accepted: {:?}", wid, e);
        }
    }

    // ["raise-window", wid]: bring the window to the front. Also arrives as the server's fallback
    // for restack requests, since we don't advertise the "window.restack" capability. Like xpra's
    // own client, skip it if the window already has focus; focus_window() is a no-op on Wayland.
    fn process_raise_window(&mut self, packet: &Packet) {
        let wid = packet.get_u64(1);
        let window = match self.windows.get(&wid) {
            Some(window) => window,
            None => {
                error!("cannot raise: window {:#x} not found", wid);
                return;
            }
        };
        if !window.window.has_focus() {
            window.window.focus_window();
        }
    }

    // ["show-desktop", show]: the server asks the client to show the desktop - xpra's own client
    // forwards this to the local window manager as EWMH `_NET_SHOWING_DESKTOP` ("minimize
    // everything to reveal the desktop"), a no-op stub off X11. We don't manage the whole client
    // desktop, so the portable equivalent within winit is to minimize (show=true) or restore
    // (show=false) our own windows; `set_minimized` works on X11, Wayland and Windows. Only arrives
    // when the server session enables it (gated by `show_desktop_allowed` server-side). A short
    // packet defaults to restore rather than indexing past the end (panic=abort).
    fn process_show_desktop(&mut self, packet: &Packet) {
        let show = packet.len() >= 2 && packet.get_bool(1);
        debug!("show-desktop: {}", show);
        for window in self.windows.values() {
            window.window.set_minimized(show);
        }
    }

    // ["pointer-position", wid, x, y, rx, ry]: the server reporting where the pointer is on its
    // side. Shadow / desktop-forwarding sessions poll the real pointer and push its position here
    // so the client can draw a "remote pointer" overlay (xpra's own show_pointer_overlay). We
    // render no such overlay, so just log it; rx/ry (root-relative) are omitted by pre-v5 senders.
    // Guarded against a short packet (panic=abort).
    fn process_pointer_position(&self, packet: &Packet) {
        if packet.len() < 4 {
            debug!("pointer-position: {:?}", &packet.main[1..]);
            return;
        }
        let wid = packet.get_u64(1);
        let (x, y) = (packet.get_i32(2), packet.get_i32(3));
        let (rx, ry) = if packet.len() >= 6 {
            (packet.get_i32(4), packet.get_i32(5))
        } else {
            (-1, -1)
        };
        debug!("pointer-position: {},{} ({},{} relative to window {:#x})", x, y, rx, ry, wid);
    }

    // ["pointer-grab", wid]: a remote application has grabbed its pointer. Prefer confining the
    // cursor to the forwarded window; some winit backends only implement locking, so use that as
    // the fallback. If another window held the grab, release it first.
    fn process_pointer_grab(&mut self, packet: &Packet) {
        if packet.len() < 2 {
            warn!("ignoring malformed pointer-grab packet with no window id");
            return;
        }
        let wid = packet.get_u64(1);
        if self.pointer_grabbed == Some(wid) {
            return;
        }
        if self.pointer_grabbed.is_some() {
            self.release_pointer_grab();
        }
        let window = match self.windows.get(&wid) {
            Some(window) => &window.window,
            None => {
                warn!("cannot grab pointer: window {:#x} not found", wid);
                return;
            }
        };
        match window
            .set_cursor_grab(CursorGrabMode::Confined)
            .or_else(|_| window.set_cursor_grab(CursorGrabMode::Locked))
        {
            Ok(()) => {
                self.pointer_grabbed = Some(wid);
                debug!("pointer grabbed by window {:#x}", wid);
            }
            Err(e) => warn!("failed to grab pointer for window {:#x}: {:?}", wid, e),
        }
    }

    // ["pointer-ungrab", wid]: the wid is informational; the local windowing API has one active
    // pointer grab for the application, so release whichever forwarded window currently owns it.
    fn process_pointer_ungrab(&mut self, packet: &Packet) {
        if packet.len() >= 2 {
            debug!("pointer-ungrab requested for window {}", packet.get_i64(1));
        }
        self.release_pointer_grab();
    }

    fn release_pointer_grab(&mut self) {
        let Some(wid) = self.pointer_grabbed.take() else {
            return;
        };
        if let Some(window) = self.windows.get(&wid) {
            if let Err(e) = window.window.set_cursor_grab(CursorGrabMode::None) {
                warn!("failed to release pointer grab for window {:#x}: {:?}", wid, e);
            } else {
                debug!("pointer grab released for window {:#x}", wid);
            }
        }
    }

    // ["cursor", encoding, x, y, w, h, xhot, yhot, serial, pixels, name, ...sizes]: the pointer
    // cursor shape. xpra sends one cursor for the whole session (not per-window), so we apply it to
    // every window and remember it for windows created later. A 2-item ["cursor", ""] packet resets
    // to the default. We only advertised the "png" encoding, so pixels decode like a window icon.
    fn process_cursor(&mut self, event_loop: &ActiveEventLoop, packet: &mut Packet) {
        // an empty (2-item) packet means "use the default cursor":
        if packet.len() <= 2 {
            self.current_cursor = None;
            for window in self.windows.values() {
                window.window.set_cursor(CursorIcon::Default);
            }
            return;
        }
        // the encoding may be prefixed "default:" (also marks it as the session default); either
        // way we just render it, so strip the prefix:
        let encoding = packet.get_str(1);
        let encoding = encoding.rsplit(':').next().unwrap_or(&encoding);
        if encoding != "png" {
            debug!("ignoring cursor with unsupported encoding {:?}", encoding);
            return;
        }
        let xhot = packet.get_u32(6);
        let yhot = packet.get_u32(7);
        let data = packet.get_bytes(9);
        let (w, h, rgba) = match draw_decoder::decode_png_rgba(&data) {
            Ok(decoded) => decoded,
            Err(e) => {
                debug!("failed to decode cursor: {}", e);
                return;
            }
        };
        // winit takes u16 dimensions and a hotspot that must lie inside the image:
        let (cw, ch) = (w.min(u16::MAX as u32) as u16, h.min(u16::MAX as u32) as u16);
        let hx = xhot.min(w.saturating_sub(1)) as u16;
        let hy = yhot.min(h.saturating_sub(1)) as u16;
        let source = match CustomCursor::from_rgba(rgba, cw, ch, hx, hy) {
            Ok(source) => source,
            Err(e) => {
                debug!("invalid cursor image {}x{}: {:?}", w, h, e);
                return;
            }
        };
        let cursor = event_loop.create_custom_cursor(source);
        for window in self.windows.values() {
            window.window.set_cursor(cursor.clone());
        }
        self.current_cursor = Some(cursor);
    }

    // ["notify_show", dbus_id, nid, app_name, replaces_nid, app_icon, summary, body, expire_timeout,
    // icon, actions, hints]: a server-forwarded desktop notification. We advertised "notifications"
    // so the server sends these.
    //
    // On Windows the notification is shown as a balloon on the system tray icon (see tray.rs) -
    // that icon is already there and `Shell_NotifyIconW` needs nothing else, so notifications come
    // essentially for free. Everywhere else - and on Windows if the tray could not be created -
    // they are only logged, like xpra's own headless fallback: there is no portable notifier
    // without a D-Bus dependency, which this client avoids. The log line is kept on every platform
    // since it is also what reaches the server through remote logging.
    fn process_notify_show(&mut self, packet: &Packet) {
        let app_name = packet.get_str(3);
        let summary = packet.get_str(6);
        let body = packet.get_str(7);
        if app_name.is_empty() {
            info!("notification: {summary}");
        } else {
            info!("notification from {app_name}: {summary}");
        }
        for line in body.lines() {
            info!("  {line}");
        }
        #[cfg(windows)]
        if let Some(tray) = &mut self.tray {
            tray.show_notification(packet.get_u64(2), &app_name, &summary, &body);
        }
    }

    // ["notify_close", nid]: the server withdrawing a notification, by the same id `notify_show`
    // carried. Only the Windows balloon can actually be taken back; the log line already went out.
    fn process_notify_close(&mut self, packet: &Packet) {
        let notification_id = packet.get_u64(1);
        debug!("notification {notification_id} closed");
        #[cfg(windows)]
        if let Some(tray) = &mut self.tray {
            tray.close_notification(notification_id);
        }
    }

    // ["bell", wid, device, percent, pitch, duration, bell_class, bell_id, bell_name]: the server
    // forwarding a window's bell (e.g. a terminal's ^G). We advertised "bell" support in the hello,
    // without which the server never sends this. Only pitch/duration are used (see ring_bell).
    fn process_bell(&mut self, packet: &Packet) {
        let pitch = packet.get_i32(4);
        let duration = packet.get_i32(5);
        ring_bell(pitch, duration);
    }

    // ["window-icon", wid, w, h, encoding, pixels]: the titlebar/taskbar icon. The server only
    // ever ships icons as png (see xpra's windowicon.py), which we advertised support for in the
    // hello; a "default"/empty payload just means "keep the default icon". A bad icon logs and
    // leaves the current one in place - purely cosmetic, never fatal.
    fn process_window_icon(&mut self, packet: &mut Packet) {
        let wid = packet.get_u64(1);
        let encoding = packet.get_str(4);
        if encoding != "png" {
            debug!("ignoring window-icon for {:#x} with unsupported encoding {:?}", wid, encoding);
            return;
        }
        let data = packet.get_bytes(5);
        let (w, h, rgba) = match draw_decoder::decode_png_rgba(&data) {
            Ok(decoded) => decoded,
            Err(e) => {
                debug!("failed to decode window icon for {:#x}: {}", wid, e);
                return;
            }
        };
        let icon = match Icon::from_rgba(rgba, w, h) {
            Ok(icon) => icon,
            Err(e) => {
                debug!("invalid window icon for {:#x}: {:?}", wid, e);
                return;
            }
        };
        match self.windows.get(&wid) {
            Some(window) => window.window.set_window_icon(Some(icon)),
            None => error!("cannot set icon: window {:#x} not found", wid),
        }
    }

    fn process_lost_window(&mut self, packet: &Packet) {
        let wid = packet.get_u64(1);
        if self.pointer_grabbed == Some(wid) {
            self.release_pointer_grab();
        }
        if let Some(window) = self.windows.remove(&wid) {
            self.id_map.remove(&window.window.id());
        } else {
            warn!("window {:#x} not found!", wid);
        }
    }

    fn process_window_metadata(&mut self, packet: &Packet) {
        let wid = packet.get_u64(1);
        let metadata = &packet.main[2];
        info!("window-metadata for {:#x}: {:?}", wid, metadata);
        let window = match self.windows.get_mut(&wid) {
            Some(window) => window,
            None => {
                warn!("window {:#x} not found!", wid);
                return;
            }
        };
        Self::apply_window_metadata(window, WindowMetadataUpdate::parse(metadata));
    }

    fn apply_window_metadata(window: &mut XpraWindow, update: WindowMetadataUpdate) {
        if let Some(title) = update.title {
            window.window.set_title(&title);
        }
        if let Some(decorations) = update.decorations {
            window.window.set_decorations(decorations && !window.override_redirect);
        }
        if let Some(constraints) = update.size_constraints {
            window.window.set_min_inner_size(
                constraints.minimum.map(|(w, h)| PhysicalSize::new(w, h)),
            );
            window.window.set_max_inner_size(
                constraints.maximum.map(|(w, h)| PhysicalSize::new(w, h)),
            );
            window.window.set_resize_increments(
                constraints.increment.map(|(w, h)| PhysicalSize::new(w, h)),
            );
            let fixed_size = constraints.minimum.is_some()
                && constraints.minimum == constraints.maximum;
            window.window.set_resizable(!window.override_redirect && !fixed_size);
        }
        if let Some(fullscreen) = update.fullscreen {
            window.window.set_fullscreen(
                fullscreen.then_some(Fullscreen::Borderless(None)),
            );
        }
        if let Some(maximized) = update.maximized {
            window.window.set_maximized(maximized);
        }
        if let Some(iconic) = update.iconic {
            window.window.set_minimized(iconic);
        }
        let level_changed = update.above.is_some() || update.below.is_some();
        if let Some(above) = update.above {
            window.above = above;
            if above {
                window.below = false;
            }
        }
        if let Some(below) = update.below {
            window.below = below;
            if below {
                window.above = false;
            }
        }
        if level_changed {
            let level = if window.above {
                WindowLevel::AlwaysOnTop
            } else if window.below {
                WindowLevel::AlwaysOnBottom
            } else {
                WindowLevel::Normal
            };
            window.window.set_window_level(level);
        }
    }

    fn process_draw_decoded(&mut self, packet: &mut Packet) {
        let p = packet;
        let wid = p.get_u64(1);
        let x = p.get_i32(2);
        let y = p.get_i32(3);
        let w = p.get_u32(4);
        let h = p.get_u32(5);
        let coding = p.get_str(6);
        let pixels = p.get_bytes(7);
        let seq = p.get_u64(8);
        let decode_time_us = p.decode_time_us.unwrap_or(0) as i128;

        let window = match self.windows.get_mut(&wid) {
            Some(window) => window,
            None => {
                let message = "window not found!".to_string();
                self.send_draw_ack(seq, wid, w, h, -1, message);
                return;
            }
        };
        trace!("drawing {:?} on {:#x}", coding, wid);
        // an empty payload is a decoder warm-up frame (h264): ack it, but there's nothing to paint.
        if !pixels.is_empty() {
            window.paint(seq, x, y, w, h, &coding, &pixels);
        }

        let message = "".to_string();
        self.send_draw_ack(seq, wid, w, h, decode_time_us, message);
    }

    fn process_draw_failed(&mut self, packet: &Packet) {
        let p = packet;
        let wid = p.get_u64(1);
        let w = p.get_u32(4);
        let h = p.get_u32(5);
        let message = p.get_str(7);
        let seq = p.get_u64(8);
        self.send_draw_ack(seq, wid, w, h, -1, message);
    }

    fn process_ping(&mut self, packet: &Packet) {
        let echotime = packet.get_u64(1);
        let sid = if packet.len() >= 4 { packet.get_str(3) } else { "".to_string() };
        debug!("got ping, sending echo time={:?}", echotime);
        self.send_ping_echo(echotime, sid);
    }

    // The server's echo of a `ping` we sent (see send_ping): field 1 is the monotonic timestamp we
    // stamped it with, so `now - echoed` is the client->server round-trip. We keep it to report in
    // the ping_echo replies we send back to the server (send_ping_echo).
    fn process_ping_echo(&mut self, packet: &Packet) {
        let echoedtime = packet.get_i64(1);
        let rtt = self.start.elapsed().as_millis() as i64 - echoedtime;
        if rtt >= 0 {
            self.last_client_latency_ms = rtt;
            debug!("ping echo: client round-trip {} ms", rtt);
        }
    }

    fn handle_window_event(&mut self, wid: u64, event: WindowEvent) {
        match event {
            WindowEvent::Focused(is_focused) => {
                let override_redirect = self.windows.get(&wid).map(|w| w.override_redirect).unwrap_or(true);
                if is_focused && !override_redirect {
                    self.send_focus(wid);
                }
            }
            WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
                if let Some(window) = self.windows.get_mut(&wid) {
                    let size = window.window.inner_size();
                    window.resize(size.width, size.height);
                    let (x, y, w, h) = window.get_geometry();
                    debug!("updated window geometry: {:?},{:?},{:?},{:?}", x, y, w, h);
                    self.send_window_configure(wid, x, y, w, h);
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(window) = self.windows.get_mut(&wid) {
                    window.draw_screen();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let pos = self.windows.get_mut(&wid).map(|window| {
                    window.last_cursor = window.absolute_position(position);
                    window.last_cursor
                });
                if let Some((x, y)) = pos {
                    self.send_pointer_position(wid, x, y);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let xpra_button = match button {
                    MouseButton::Left => Some(1),
                    MouseButton::Middle => Some(2),
                    MouseButton::Right => Some(3),
                    MouseButton::Back => Some(8),
                    MouseButton::Forward => Some(9),
                    MouseButton::Other(n) => Some(n as i8),
                };
                if let (Some(button), Some(window)) = (xpra_button, self.windows.get(&wid)) {
                    let pressed = state == ElementState::Pressed;
                    let (x, y) = window.last_cursor;
                    self.send_pointer_button(wid, button, pressed, x, y);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    MouseScrollDelta::PixelDelta(pos) => (pos.x as f32, pos.y as f32),
                };
                if let Some(window) = self.windows.get(&wid) {
                    let (x, y) = window.last_cursor;
                    if dy != 0.0 {
                        let button = if dy > 0.0 { 4 } else { 5 };
                        self.send_pointer_button(wid, button, true, x, y);
                        self.send_pointer_button(wid, button, false, x, y);
                    }
                    if dx != 0.0 {
                        let button = if dx > 0.0 { 6 } else { 7 };
                        self.send_pointer_button(wid, button, true, x, y);
                        self.send_pointer_button(wid, button, false, x, y);
                    }
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                let pressed = key_event.state == ElementState::Pressed;
                let keycode = physical_key_to_xpra_keycode(key_event.physical_key);
                let keyname = key_to_xpra_keyname(&key_event.logical_key);
                let keystr = match &key_event.logical_key {
                    Key::Character(s) => s.to_string(),
                    _ => "".to_string(),
                };
                self.send_key_event(wid, keycode, &keyname, &keystr, pressed);
            }
            WindowEvent::CloseRequested => {
                self.send_window_close(wid);
            }
            _ => {
                trace!("unhandled window event {:?} on wid={:#x}", event, wid);
            }
        }
    }

}


impl ApplicationHandler<Packet> for XpraClient {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.softbuffer_ctx.is_none() {
            let context = Context::new(event_loop.owned_display_handle())
                .expect("failed to create softbuffer context");
            self.softbuffer_ctx = Some(context);
        }
        // the tray window has to be created on this (the UI) thread, whose message loop winit runs
        // and which therefore pumps it - see tray.rs. Not having a tray is not fatal, the same
        // stance clipboard.rs takes when there is no usable clipboard.
        #[cfg(windows)]
        if self.tray.is_none() {
            match tray::Tray::new(self.proxy.clone(), &self.target) {
                Ok(tray) => self.tray = Some(tray),
                Err(e) => warn!("system tray unavailable: {}", e),
            }
        }
        if !self.hello_sent {
            // measured here because this is the first callback that hands us an `ActiveEventLoop`,
            // which is what winit enumerates monitors through. Kept on `self` so the second hello
            // that answers an authentication challenge reports the same layout.
            self.monitors = local_monitors(event_loop);
            self.desktop_size = total_display_size(&self.monitors);
            match self.desktop_size {
                Some((w, h)) => info!("local display size: {w}x{h}"),
                None => warn!("no local display size to report to the server"),
            }
            for (index, monitor) in self.monitors.iter().enumerate() {
                let (x, y, w, h) = monitor.geometry;
                let primary = if monitor.primary { " (primary)" } else { "" };
                info!("monitor {index} {:?}: {w}x{h} at {x},{y}{primary}", monitor.name);
            }
            self.start_read_loop();
            self.hello_sent = true;
            self.send_hello(None);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, packet: Packet) {
        if packet.len() == 0 {
            error!("empty packet!");
            return;
        }
        let packet_type = packet.get_str(0);
        if packet_type.is_empty() {
            error!("malformed packet");
            return;
        }
        self.do_process_packet(event_loop, &packet_type, packet);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        // the password dialog is not an xpra window (no wid); route its events separately.
        if self.auth_dialog.as_ref().map(|d| d.window.id()) == Some(window_id) {
            self.handle_auth_dialog_event(event_loop, event);
            return;
        }
        let Some(&wid) = self.id_map.get(&window_id) else {
            trace!("window event for unknown window {:?}", window_id);
            return;
        };
        self.handle_window_event(wid, event);
    }
}


fn physical_key_to_xpra_keycode(physical_key: PhysicalKey) -> u32 {
    match physical_key.to_scancode() {
        // X11/Wayland: linux scancode -> X11/XKB keycode is scancode + 8.
        #[cfg(target_os = "linux")]
        Some(scancode) => scancode + 8,
        #[cfg(not(target_os = "linux"))]
        Some(scancode) => scancode,
        None => 0,
    }
}

fn key_to_xpra_keyname(key: &Key) -> String {
    match key {
        // most printable characters (letters, digits) are their own X11 keysym name,
        // but punctuation has dedicated symbolic keysym names:
        Key::Character(s) => match s.as_str() {
            "-" => "minus",
            "=" => "equal",
            "," => "comma",
            "." => "period",
            "/" => "slash",
            ";" => "semicolon",
            "'" => "apostrophe",
            "`" => "grave",
            "[" => "bracketleft",
            "]" => "bracketright",
            "\\" => "backslash",
            other => other,
        }.to_string(),
        Key::Named(named) => match named {
            NamedKey::Enter => "Return",
            NamedKey::Tab => "Tab",
            NamedKey::Space => "space",
            NamedKey::Backspace => "BackSpace",
            NamedKey::Delete => "Delete",
            NamedKey::Escape => "Escape",
            NamedKey::ArrowUp => "Up",
            NamedKey::ArrowDown => "Down",
            NamedKey::ArrowLeft => "Left",
            NamedKey::ArrowRight => "Right",
            NamedKey::Home => "Home",
            NamedKey::End => "End",
            NamedKey::PageUp => "Prior",
            NamedKey::PageDown => "Next",
            NamedKey::Insert => "Insert",
            NamedKey::Shift => "shift",
            NamedKey::Control => "control",
            NamedKey::Alt => "mod1",
            NamedKey::AltGraph => "mod5",
            NamedKey::Super => "super",
            NamedKey::CapsLock => "Caps_Lock",
            NamedKey::NumLock => "Num_Lock",
            NamedKey::ScrollLock => "Scroll_Lock",
            NamedKey::F1 => "F1", NamedKey::F2 => "F2", NamedKey::F3 => "F3", NamedKey::F4 => "F4",
            NamedKey::F5 => "F5", NamedKey::F6 => "F6", NamedKey::F7 => "F7", NamedKey::F8 => "F8",
            NamedKey::F9 => "F9", NamedKey::F10 => "F10", NamedKey::F11 => "F11", NamedKey::F12 => "F12",
            NamedKey::ContextMenu => "Menu",
            NamedKey::PrintScreen => "Print",
            NamedKey::Pause => "Pause",
            _ => "",
        }.to_string(),
        _ => "".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        draw_ack_packet, server_backwards_compatible, WindowMetadataUpdate, WindowSizeConstraints,
    };
    use serde_json::json;
    use yaml_rust2::YamlLoader;

    fn parse_metadata(yaml: &str) -> WindowMetadataUpdate {
        let documents = YamlLoader::load_from_str(yaml).unwrap();
        WindowMetadataUpdate::parse(&documents[0])
    }

    #[test]
    fn parse_window_metadata_fields() {
        let metadata = parse_metadata(
            r#"{
                title: "Terminal",
                decorations: false,
                fullscreen: true,
                maximized: 1,
                iconic: 0,
                above: true,
                below: false,
                size-constraints: {
                    minimum-size: [320, 200],
                    maximum-size: [1920, 1080],
                    increment: [8, 16]
                }
            }"#,
        );
        assert_eq!(
            metadata,
            WindowMetadataUpdate {
                title: Some("Terminal".to_string()),
                decorations: Some(false),
                fullscreen: Some(true),
                maximized: Some(true),
                iconic: Some(false),
                above: Some(true),
                below: Some(false),
                size_constraints: Some(WindowSizeConstraints {
                    minimum: Some((320, 200)),
                    maximum: Some((1920, 1080)),
                    increment: Some((8, 16)),
                }),
            }
        );
    }

    #[test]
    fn parse_partial_window_metadata_preserves_absence() {
        let metadata = parse_metadata(
            r#"{
                title: "Updated",
                size-constraints: {
                    minimum-size: [0, 0],
                    maximum-size: [640]
                }
            }"#,
        );
        assert_eq!(metadata.title.as_deref(), Some("Updated"));
        assert_eq!(metadata.fullscreen, None);
        assert_eq!(metadata.decorations, None);
        assert_eq!(
            metadata.size_constraints,
            Some(WindowSizeConstraints::default())
        );
    }

    #[test]
    fn draw_ack_uses_legacy_layout_in_backwards_compatible_mode() {
        assert_eq!(
            draw_ack_packet(true, 17, 3, 640, 480, 2500, "decoded".to_string()),
            json!(["window-draw-ack", 17, 3, 640, 480, 2500, "decoded"]),
        );
    }

    #[test]
    fn draw_ack_uses_window_ack_layout_in_modern_mode() {
        assert_eq!(
            draw_ack_packet(false, 17, 3, 640, 480, 2500, "decoded".to_string()),
            json!(["window-ack", 3, 640, 480, 17, 2500, "decoded"]),
        );
    }

    #[test]
    fn packet_types_identify_server_compatibility_mode() {
        let compatible = YamlLoader::load_from_str(
            "{packet-types: [window-ack, window-draw-ack, damage-sequence]}",
        ).unwrap();
        let modern = YamlLoader::load_from_str(
            "{packet-types: [window-ack, window-draw-ack]}",
        ).unwrap();
        let unspecified = YamlLoader::load_from_str("{version: 6.6}").unwrap();

        assert_eq!(server_backwards_compatible(&compatible[0]), Some(true));
        assert_eq!(server_backwards_compatible(&modern[0]), Some(false));
        assert_eq!(server_backwards_compatible(&unspecified[0]), None);
    }
}
