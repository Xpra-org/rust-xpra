use alloc::string::ToString;
use machine_uid;

use std::env;
use std::fmt;
use std::io;
use std::rc::Rc;
use std::collections::HashMap;
use std::sync::mpsc::{Sender, Receiver};
use std::thread;
use std::time::Instant;

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
use winit::window::{ResizeDirection, Window, WindowId};

use xpra::exit_codes::ExitCode;
use xpra::net::serde::VERSION_KEY_STR;
use xpra::VERSION;
use xpra::net::connection::Connection;
use xpra::net::io::{write_packet, read_packet};
use xpra::net::serde::parse_payload;
use xpra::net::packet::Packet;
use super::draw_decoder;
use super::window::XpraWindow;


pub struct XpraClient {
    pub hello_sent: bool,
    pub server_version: String,
    pub windows: HashMap<u64, XpraWindow>,
    pub id_map: HashMap<WindowId, u64>,
    pub stream: Connection,
    pub proxy: EventLoopProxy<Packet>,
    pub decode_sender: Sender<Packet>,
    pub softbuffer_ctx: Option<Context<OwnedDisplayHandle>>,
    pub modifiers: ModifiersState,
    pub startup_complete: bool,
    // `Some` once we're on the way out (a `disconnect` packet, a lost connection or a failed
    // write): it holds the code we'll exit the process with, and stops us from writing to (and
    // complaining about) a dead connection while the event loop winds down.
    pub exit_code: Option<ExitCode>,
}


// "connection-lost" and "invalid-packet" are client-side packet types (like "draw-decoded"):
// the reader thread and the write path use them to tell the UI thread that the connection is
// gone, since only `user_event` has access to the `ActiveEventLoop` needed to stop the event loop.
fn client_packet(packet_type: &str, message: &str) -> Packet {
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

fn connection_error(e: &io::Error) -> String {
    match e.kind() {
        // what a killed server looks like: the socket closed mid-packet (or between packets),
        // and "failed to fill whole buffer" is not a helpful thing to show the user.
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset =>
            "connection closed by the server".to_string(),
        _ => e.to_string(),
    }
}

impl fmt::Debug for XpraClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XpraClient")
            .field("server", &self.server_version)
            .finish()
    }
}

impl XpraClient {

    pub fn new(stream: Connection, proxy: EventLoopProxy<Packet>, decode_sender: Sender<Packet>) -> Self {
        XpraClient {
            hello_sent: false,
            server_version: "".to_string(),
            windows: HashMap::new(),
            id_map: HashMap::new(),
            stream,
            proxy,
            decode_sender,
            softbuffer_ctx: None,
            modifiers: ModifiersState::empty(),
            startup_complete: false,
            exit_code: None,
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

    pub fn send_hello(&mut self) {
        let platform = match std::env::consts::OS {
            "windows" => "win32",
            "macos" => "darwin",
            other => other,
        };
        let username = env::var("USERNAME").or_else(|_| env::var("USER")).unwrap_or_default();
        // h264 is decoded via Media Foundation, which is Windows-only:
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut encodings = vec!["jpeg", "png", "webp"];
        // The nested "encoding" caps dict (read server-side as hello["encoding"], see xpra's
        // server/source/encoding.py). For a video encoding to be offered at all, the server needs
        // `full_csc_modes[<enc>]` to list at least one colourspace its encoder can produce that we
        // can decode. Media Foundation's H.264 decoder only handles 8-bit 4:2:0 up to High profile,
        // so we advertise *only* YUV420P (never 422/444) and pin the profile to "high".
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut encoding_caps = json!({});
        #[cfg(windows)]
        {
            encodings.push("h264");
            encoding_caps = json!({
                "full_csc_modes": { "h264": ["YUV420P"] },
                "h264": { "YUV420P.profile": "high" },
            });
        }
        let packet = json!(["hello", {
            "version": VERSION,
            "yaml": true,
            "chunks": false,
            "windows": true,
            "keyboard": true,
            "mouse": true,
            "sharing": true,
            "ping": true,
            "encodings": encodings,
            "encoding": encoding_caps,
            "client_type": "rust",
            "platform": platform,
            "user": env::var("USER").unwrap_or("".into()),
            "username": username,
            "hostname": env::var("HOSTNAME").unwrap_or("".into()),
            "uuid": machine_uid::get().unwrap(),
        }]);
        self.write_json(packet);
    }

    pub fn send_focus(&mut self, wid: u64) {
        let packet = json!(["focus", wid]);
        self.write_json(packet);
    }

    fn send_pointer_position(&mut self, wid: u64, x: i32, y: i32) {
        let device_id = 0;
        let sequence = 0;
        let packet = json!(["pointer", device_id, sequence, wid, [x, y], {}]);
        self.write_json(packet);
    }

    fn send_pointer_button(&mut self, wid: u64, button: i8, pressed: bool, x: i32, y: i32) {
        let device_id = 0;
        let sequence = 0;
        let packet = json!(["pointer-button", device_id, sequence, wid, button, pressed, [x, y], {}]);
        self.write_json(packet);
    }

    fn send_key_event(&mut self, wid: u64, keycode: u32, keyname: &str, keystr: &str, pressed: bool) {
        let modifiers = self.get_modifier_state();
        let group = 0;
        let packet = json!(["key-action", wid, keyname, pressed, modifiers, 0, keystr, keycode, group]);
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

    fn send_window_map(&mut self, wid: u64, x: i32, y: i32, w: u32, h: u32) {
        let packet = json!(["map-window", wid, x, y, w, h, {}, {}]);
        self.write_json(packet);
    }

    fn send_window_configure(&mut self, wid: u64, x: i32, y: i32, w: u32, h: u32) {
        let packet = json!(["configure-window", wid, x, y, w, h, {}]);
        self.write_json(packet);
    }

    fn send_window_close(&mut self, wid: u64) {
        let packet = json!(["close-window", wid]);
        self.write_json(packet);
    }

    fn send_damage_sequence(&mut self, seq: u64, wid: u64, w: u32, h: u32, decode_time: i128, message: String) {
        let packet = json!(["damage-sequence", seq, wid, w, h, decode_time, message]);
        self.write_json(packet);
    }

    fn send_ping_echo(&mut self, echotime: u64, sid: String) {
        // no load average or client-side ping latency tracked (we don't send our own pings):
        let packet = json!(["ping_echo", echotime, 0, 0, 0, -1, sid]);
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
            let payload = match read_packet(&mut stream) {
                Ok(payload) => payload,
                Err(e) => {
                    // the server closed the connection (or died): hand the reason to the UI
                    // thread, which logs it and exits the event loop.
                    debug!("read loop terminated: {}", e);
                    let _ = proxy.send_event(client_packet("connection-lost", &connection_error(&e)));
                    break;
                }
            };
            let read_elapsed = t0.elapsed();
            let payload_len = payload.len();
            let packet = match parse_payload(payload) {
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


    pub fn start_draw_decode_loop(proxy: EventLoopProxy<Packet>, receiver: Receiver<Packet>) {
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
                // window teardown forwarded from the UI thread: release its h264 decoder (Windows).
                if packet.get_str(0) == "lost-window" {
                    #[cfg(windows)]
                    {
                        let key = packet.get_u64(1);
                        if h264_decoders.remove(&key).is_some() {
                            debug!("released h264 decoder for lost window {:?}", key);
                        }
                    }
                    continue;
                }
                let wid = packet.get_i64(1);
                let w = packet.get_i32(4);
                let h = packet.get_i32(5);
                let coding = packet.get_str(6);
                let data = packet.get_bytes(7);
                let seq = packet.get_i64(8);
                debug!("wid {:?} got {:?}x{:?} {:?} draw packet", wid, w, h, coding);

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
                self.process_hello(&p.main[1]);
            }
            "encodings" => debug!("got server encodings: {:?}", p.main[1]),
            "startup-complete" => {
                info!("startup complete!");
                self.startup_complete = true;
            }
            "new-window" => self.process_new_common(event_loop, &p, false),
            "new-override-redirect" => self.process_new_common(event_loop, &p, true),
            "window-move-resize" => self.process_window_move_resize(&p),
            "initiate-moveresize" => self.process_initiate_moveresize(&p),
            "lost-window" => {
                self.process_lost_window(&p);
                // forward to the decode thread so it can drop this window's persistent h264
                // decoder; routed through the same channel as draws, so any still-queued draws
                // for this window drain before the decoder is released.
                #[cfg(windows)]
                { let _ = self.decode_sender.send(p); }
            }
            "window-metadata" => self.process_window_metadata(&p),
            "draw" => {
                if self.decode_sender.send(p).is_err() {
                    error!("cannot decode: the decoding thread has stopped");
                }
            }
            "draw-decoded" => self.process_draw_decoded(&mut p),
            "draw-failed" => self.process_draw_failed(&p),
            "ping" => self.process_ping(&p),
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

    fn process_hello(&mut self, hello: &Yaml) {
        match &hello {
            Yaml::Hash(hash) => {
                let version_key: Yaml = Yaml::String(VERSION_KEY_STR.to_string());
                let version = &hash[&version_key];
                if let Yaml::String(version_str) = version {
                    info!("server version {:?}", version_str);
                    self.server_version = version_str.to_string();
                }
            },
            _ => error!("unexpected hello data type: {:?}", hello),
        }
    }

    fn process_new_common(&mut self, event_loop: &ActiveEventLoop, packet: &Packet, override_redirect: bool) {
        let wid = packet.get_u64(1);
        debug!("new-window {:?}, override-redirect={:?}", wid, override_redirect);
        let x = packet.get_i32(2);
        let y = packet.get_i32(3);
        let w = packet.get_u32(4);
        let h = packet.get_u32(5);
        let title = packet.get_hash_str(6, "title".to_string());
        // override-redirect windows are never decorated; otherwise honour the metadata flag
        // (absent means decorated, as in xpra's own client - see `client/gui/window_base.py`)
        let decorated = !override_redirect
            && packet.get_hash_bool(6, "decorations".to_string()).unwrap_or(true);

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
        info!("new-window {:?} : {:?}", wid, title);

        let context = self.softbuffer_ctx.as_ref().expect("softbuffer context not initialized");
        let mut xpra_window = XpraWindow::new(wid, window.clone(), context, w, h, override_redirect);
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
                error!("cannot move-resize: window {:?} not found", wid);
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
            debug!("window {:?}: absolute positioning is not supported on this platform (Wayland)", wid);
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
                error!("cannot initiate move-resize: window {:?} not found", wid);
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
            debug!("initiate-moveresize for window {:?} was not accepted: {:?}", wid, e);
        }
    }

    fn process_lost_window(&mut self, packet: &Packet) {
        let wid = packet.get_u64(1);
        if let Some(window) = self.windows.remove(&wid) {
            self.id_map.remove(&window.window.id());
        } else {
            warn!("window {:?} not found!", wid);
        }
    }

    fn process_window_metadata(&mut self, packet: &Packet) {
        let wid = packet.get_u64(1);
        let metadata = &packet.main[2];
        info!("window-metadata for {:?}: {:?}", wid, metadata);
        let window = match self.windows.get(&wid) {
            Some(window) => window,
            None => {
                warn!("window {:?} not found!", wid);
                return;
            }
        };
        if let Some(decorations) = packet.get_hash_bool(2, "decorations".to_string()) {
            window.window.set_decorations(decorations && !window.override_redirect);
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
                self.send_damage_sequence(seq, wid, w, h, -1, message);
                return;
            }
        };
        trace!("drawing {:?} on {:?}", coding, wid);
        // an empty payload is a decoder warm-up frame (h264): ack it, but there's nothing to paint.
        if !pixels.is_empty() {
            window.paint(seq, x, y, w, h, &coding, &pixels);
        }

        let message = "".to_string();
        self.send_damage_sequence(seq, wid, w, h, decode_time_us, message);
    }

    fn process_draw_failed(&mut self, packet: &Packet) {
        let p = packet;
        let wid = p.get_u64(1);
        let w = p.get_u32(4);
        let h = p.get_u32(5);
        let message = p.get_str(7);
        let seq = p.get_u64(8);
        self.send_damage_sequence(seq, wid, w, h, -1, message);
    }

    fn process_ping(&mut self, packet: &Packet) {
        let echotime = packet.get_u64(1);
        let sid = if packet.len() >= 4 { packet.get_str(3) } else { "".to_string() };
        debug!("got ping, sending echo time={:?}", echotime);
        self.send_ping_echo(echotime, sid);
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
                trace!("unhandled window event {:?} on wid={:?}", event, wid);
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
        if !self.hello_sent {
            self.start_read_loop();
            self.hello_sent = true;
            self.send_hello();
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

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
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
