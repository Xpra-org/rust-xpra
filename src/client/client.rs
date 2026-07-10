use alloc::string::ToString;
use machine_uid;

use std::env;
use std::fmt;
use std::rc::Rc;
use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::mpsc::{Sender, Receiver};
use std::thread;

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
use winit::window::{Window, WindowId};

use xpra::net::serde::VERSION_KEY_STR;
use xpra::VERSION;
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
    pub stream: TcpStream,
    pub proxy: EventLoopProxy<Packet>,
    pub decode_sender: Sender<Packet>,
    pub softbuffer_ctx: Option<Context<OwnedDisplayHandle>>,
    pub modifiers: ModifiersState,
}

impl fmt::Debug for XpraClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XpraClient")
            .field("server", &self.server_version)
            .finish()
    }
}

impl XpraClient {

    pub fn new(stream: TcpStream, proxy: EventLoopProxy<Packet>, decode_sender: Sender<Packet>) -> Self {
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
        }
    }

    pub fn send_hello(&self) {
        let platform = match std::env::consts::OS {
            "windows" => "win32",
            "macos" => "darwin",
            other => other,
        };
        let username = env::var("USERNAME").or_else(|_| env::var("USER")).unwrap_or_default();
        let packet = json!(["hello", {
            "version": VERSION,
            "yaml": true,
            "chunks": false,
            "windows": true,
            "keyboard": true,
            "mouse": true,
            "sharing": true,
            "ping": true,
            "encodings": ["jpeg", "png", ],
            "client_type": "rust",
            "platform": platform,
            "user": env::var("USER").unwrap_or("".into()),
            "username": username,
            "hostname": env::var("HOSTNAME").unwrap_or("".into()),
            "uuid": machine_uid::get().unwrap(),
        }]);
        self.write_json(packet);
    }

    pub fn send_focus(&self, wid: u64) {
        let packet = json!(["focus", wid]);
        self.write_json(packet);
    }

    fn send_pointer_position(&self, wid: u64, x: i32, y: i32) {
        let device_id = 0;
        let sequence = 0;
        let packet = json!(["pointer", device_id, sequence, wid, [x, y], {}]);
        self.write_json(packet);
    }

    fn send_pointer_button(&self, wid: u64, button: i8, pressed: bool, x: i32, y: i32) {
        let device_id = 0;
        let sequence = 0;
        let packet = json!(["pointer-button", device_id, sequence, wid, button, pressed, [x, y], {}]);
        self.write_json(packet);
    }

    fn send_key_event(&self, wid: u64, keycode: u32, keyname: &str, keystr: &str, pressed: bool) {
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

    fn send_window_map(&self, wid: u64, x: i32, y: i32, w: u32, h: u32) {
        let packet = json!(["map-window", wid, x, y, w, h, {}, {}]);
        self.write_json(packet);
    }

    fn send_window_configure(&self, wid: u64, x: i32, y: i32, w: u32, h: u32) {
        let packet = json!(["configure-window", wid, x, y, w, h, {}]);
        self.write_json(packet);
    }

    fn send_window_close(&self, wid: u64) {
        let packet = json!(["close-window", wid]);
        self.write_json(packet);
    }

    fn send_damage_sequence(&self, seq: u64, wid: u64, w: u32, h: u32, decode_time: i128, message: String) {
        let packet = json!(["damage-sequence", seq, wid, w, h, decode_time, message]);
        self.write_json(packet);
    }

    fn send_ping_echo(&self, echotime: u64, sid: String) {
        // no load average or client-side ping latency tracked (we don't send our own pings):
        let packet = json!(["ping_echo", echotime, 0, 0, 0, -1, sid]);
        self.write_json(packet);
    }

    fn write_json(&self, packet: Value) {
        let packet_str = packet.to_string();
        let packet_data = packet_str.as_bytes();
        write_packet(&self.stream, packet_data);
    }


    pub fn start_read_loop(&mut self) {
        let proxy = self.proxy.clone();
        let stream = self.stream.try_clone().unwrap();
        thread::Builder::new().name("reader".to_string()).spawn(move || loop {
            let payload = read_packet(&stream).unwrap();
            let packet = parse_payload(payload).unwrap();
            if proxy.send_event(packet).is_err() {
                break;
            }
        }).unwrap();
    }


    pub fn start_draw_decode_loop(proxy: EventLoopProxy<Packet>, receiver: Receiver<Packet>) {
        thread::Builder::new().name("decode".to_string()).spawn(move || {
            info!("decoding thread started");
            loop {
                let mut packet = receiver.recv().unwrap();
                let wid = packet.get_i64(1);
                let w = packet.get_i32(4);
                let h = packet.get_i32(5);
                let coding = packet.get_str(6);
                let data = packet.get_bytes(7);
                let seq = packet.get_i64(8);
                debug!("wid {:?} got {:?}x{:?} {:?} draw packet", wid, w, h, coding);

                let mut main = packet.main.to_vec();
                let mut raw = HashMap::new();
                let result = draw_decoder::decode(&coding, data);
                if result.is_err() {
                    let message = result.unwrap_err();
                    error!("draw decoding error for {:?} sequence {:?}: {:?}", coding, seq, message);
                    main[0] = Yaml::String("decoding-failed".to_string());
                    main[7] = Yaml::String(message.to_string());
                }
                else {
                    let pixels = result.unwrap();
                    raw.insert(7, pixels);
                    main[0] = Yaml::String("draw-decoded".to_string());
                }
                let patched_packet = Packet { main, raw };
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
            "startup-complete" => info!("startup complete!"),
            "new-window" => self.process_new_common(event_loop, &p, false),
            "new-override-redirect" => self.process_new_common(event_loop, &p, true),
            "window-move-resize" => self.process_window_move_resize(&p),
            "lost-window" => self.process_lost_window(&p),
            "window-metadata" => self.process_window_metadata(&p),
            "draw" => { self.decode_sender.send(p).unwrap(); }
            "draw-decoded" => self.process_draw_decoded(&mut p),
            "draw-failed" => self.process_draw_failed(&p),
            "ping" => self.process_ping(&p),
            "disconnect" => event_loop.exit(),
            other => warn!("unhandled packet type {:?}", other),
        }
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

        let mut attrs = Window::default_attributes()
            .with_title(&title)
            .with_position(PhysicalPosition::new(x, y))
            .with_inner_size(PhysicalSize::new(w.max(1), h.max(1)))
            .with_decorations(!override_redirect)
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

        let window = match self.windows.get_mut(&wid) {
            Some(window) => window,
            None => {
                let message = "window not found!".to_string();
                self.send_damage_sequence(seq, wid, w, h, -1, message);
                return;
            }
        };
        trace!("drawing {:?} on {:?}", coding, wid);
        window.paint(seq, x, y, w, h, &coding, &pixels);

        let message = "".to_string();
        self.send_damage_sequence(seq, wid, w, h, 0, message);
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

    fn process_ping(&self, packet: &Packet) {
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
                if let Some(window) = self.windows.get(&wid) {
                    let (x, y) = self.absolute_cursor_position(window, position);
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
                    let (x, y) = self.last_cursor_position(window);
                    self.send_pointer_button(wid, button, pressed, x, y);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    MouseScrollDelta::PixelDelta(pos) => (pos.x as f32, pos.y as f32),
                };
                if let Some(window) = self.windows.get(&wid) {
                    let (x, y) = self.last_cursor_position(window);
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

    fn absolute_cursor_position(&self, window: &XpraWindow, position: PhysicalPosition<f64>) -> (i32, i32) {
        match window.window.inner_position() {
            Ok(origin) => (origin.x + position.x as i32, origin.y + position.y as i32),
            // not available on Wayland: fall back to window-relative coordinates.
            Err(_) => (position.x as i32, position.y as i32),
        }
    }

    fn last_cursor_position(&self, window: &XpraWindow) -> (i32, i32) {
        match window.window.inner_position() {
            Ok(origin) => (origin.x, origin.y),
            Err(_) => (0, 0),
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
