extern crate native_windows_gui as nwg;

use alloc::string::ToString;
use machine_uid;
use std::rc::Rc;
use std::collections::HashMap;
use std::net::{TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime};
use std::sync::mpsc::{Sender, Receiver};
use std::thread;
use std::cmp::max;

use std::io::{Error, ErrorKind};


//use winapi::um::winuser::DefWindowProcW;
use serde_json::{json, Value};
use yaml_rust2::{Yaml};
use log::{debug, info, warn, error};
use xpra::net::serde::{
    VERSION_KEY_STR,
};

use xpra::VERSION;
use xpra::net::io::{write_packet, read_packet};
use xpra::net::serde::{ parse_payload };
use xpra::net::packet::Packet;
use super::draw_decoder;
use super::window::{XpraWindow};


pub struct XpraClient {
    pub hello_sent: bool,
    pub server_version: String,
    pub windows: HashMap<i64, XpraWindow>,
    pub stream: TcpStream,
    pub lock: Option<Arc<Mutex<XpraClient>>>,
    pub notice: nwg::Notice,
    pub packet_sender: Sender<Packet>,
    pub decode_sender: Sender<Packet>,
}

impl XpraClient {

    pub fn send_hello(&self) {

        let packet = json!(["hello", {
            "version": VERSION,
            "yaml": true,
            "chunks": false,
            "windows": true,
            "keyboard": true,
            "mouse": true,
            "sharing": true,
            "encodings": ["png", "jpeg", ],
            "client_type": "rust",
            "platform": "win32",
            "user": env::var("USER").unwrap_or("".into()),
            "username": env::var("USERNAME").unwrap_or("".into()),
            "hostname": env::var("HOSTNAME").unwrap_or("".into()),
            "uuid": machine_uid::get().unwrap(),
        }]);
        self.write_json(packet);
    }

    fn send_pointer_position(&self, wid: i64, x: i32, y: i32) {
        let device_id = 0;
        let sequence = 0;
        let packet = json!(["pointer", device_id, sequence, wid, [x, y], {}]);
        self.write_json(packet);
    }

    fn send_pointer_button(&self, wid: i64, button: i8, pressed: bool, x: i32, y: i32) {
        let device_id = 0;
        let sequence = 0;
        let packet = json!(["pointer-button", device_id, sequence, wid, button, pressed, [x, y], {}]);
        self.write_json(packet);
    }

    fn send_key_event(&self, wid: i64, keycode: &u32, pressed: bool) {
        // use windows_sys::Win32::UI::Input::KeyboardAndMouse::MapVirtualKeyA;
        use winapi::um::winuser::{ MapVirtualKeyA, GetKeyNameTextA, VK_RETURN };
        let keystr;
        let scancode;
        let mut buf = vec![0u8; 128];
        unsafe {
            keystr = char::from_u32(MapVirtualKeyA(*keycode, 2));   // MAPVK_TO_CHAR = 2
            scancode = MapVirtualKeyA(*keycode, 0);                 // MAPVK_TO_VSC = 0
            GetKeyNameTextA((scancode << 16) as i32, buf.as_mut_ptr() as *mut i8, 127);
        }
        let mut keyname = String::from_utf8(buf).unwrap();
        keyname = keyname.trim_matches(char::from(0)).to_string();
        if *keycode == VK_RETURN as u32{
            keyname = "Return".to_string();
        }
        let group = 0;
        let packet = json!(["key-action", wid, keyname, pressed, [], 0, keystr, keycode, group]);
        self.write_json(packet);
    }

    fn send_window_map(&self, wid: i64, x: i32, y: i32, w: u32, h: u32) {
        let packet = json!(["map-window", wid, x, y, w, h, {}, {}]);
        self.write_json(packet);
    }

    fn send_window_close(&self, wid: i64) {
        let packet = json!(["close-window", wid]);
        self.write_json(packet);
    }

    fn send_damage_sequence(&self, seq: i64, wid: i64, w: i32, h: i32, decode_time: i128, message: String) {
        // send ack:
        let packet = json!(["damage-sequence", seq, wid, w, h, decode_time, message]);
        self.write_json(packet);
    }


    fn write_json(&self, packet: Value) {
        // should use yaml instead?
        let packet_str = packet.to_string();
        let packet_data = packet_str.as_bytes();
        write_packet(&self.stream, packet_data);
    }


    pub fn start_read_loop(&mut self) {
        let packet_sender = self.packet_sender.clone();
        let notice_sender = self.notice.sender();
        let stream = self.stream.try_clone().unwrap();
        thread::spawn(move || loop {
            loop {
                let payload = read_packet(&stream).unwrap();
                let packet = parse_payload(payload).unwrap();
                // send the packet to the UI thread:
                packet_sender.send(packet).unwrap();
                // notify UI thread:
                notice_sender.notice();
            }
        });
    }


    pub fn start_draw_decode_loop(&self, receiver: Receiver<Packet>) {
        let packet_sender = self.packet_sender.clone();
        let notice_sender = self.notice.sender();
        info!("draw loop starting");
        thread::spawn(move || loop {
            loop {
                let mut packet = receiver.recv().unwrap();
                let wid = packet.get_i64(1);
                let w = packet.get_i32(4);
                let h = packet.get_i32(5);
                let coding = packet.get_str(6);
                let data = packet.get_bytes(7);
                let seq = packet.get_i64(8);
                debug!("wid {:?} got {:?}x{:?} {:?} draw packet", wid, w, h, coding);

                let result = draw_decoder::decode(&coding, data);
                if result.is_err() {
                    let message = result.unwrap_err();
                    error!("draw decoding error for {:?} sequence {:?}: {:?}", coding, seq, message);
                    // self.send_damage_sequence(seq, wid, w, h, -1, message);
                    return;
                }
                let pixels = result.unwrap();
                let mut raw = HashMap::new();
                raw.insert(7, pixels);
                let mut main = packet.main.to_vec();
                main[0] = Yaml::String("decoded-draw".to_string());
                let patched_packet = Packet { main, raw };
                // send it back to the UI thread, but as 'decoded-draw'
                packet_sender.send(patched_packet).unwrap();
                notice_sender.notice();
            }
        });
    }


    pub fn process_packet(&mut self, packet: Box<Packet>) -> Result<(), Error> {
        if packet.len() == 0 {
            return Err(Error::new(ErrorKind::InvalidData, "empty packet!"));
        }
        let packet_type = packet.get_str(0);
        if packet_type != "" {
            self.do_process_packet(&packet_type, packet);
        }
        else {
            error!("malformed packet");
            return Err(Error::new(ErrorKind::InvalidData, "missing packet type!"));
        }
        Ok(())
    }

    fn do_process_packet(&mut self, packet_type: &String, packet: Box<Packet>) {
        let mut p = *packet;
        if packet_type == "hello" {
            assert!(p.len() > 1);
            self.process_hello(&p.main[1]);
        } else if packet_type == "encodings" {
            debug!("got server encodings: {:?}", p.main[1]);
        } else if packet_type == "startup-complete" {
            info!("startup complete!");
        } else if packet_type == "new-window" {
            self.process_new_window(&p)
        } else if packet_type == "lost-window" {
            self.process_lost_window(&p)
        } else if packet_type == "window-metadata" {
            self.process_window_metadata(&p)
        } else if packet_type == "draw" {
            // send the packet to the decode thread:
            self.decode_sender.send(p).unwrap();
        } else if packet_type == "decoded-draw" {
            self.process_decoded_draw(&mut p)
        } else {
            warn!("unhandled packet type {:?}", packet_type);
        }
    }


    fn process_hello(&mut self, hello: &Yaml) {
        match &hello {
            Yaml::Hash(hash) => {
                //hash
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

    fn process_new_window(&mut self, packet: &Packet) {
        let wid = packet.get_i64(1);
        debug!("new-window {:?}", wid);
        let x = packet.get_i32(2);
        let y = packet.get_i32(3);
        let w = packet.get_u32(4);
        let h = packet.get_u32(5);
        let title = packet.get_hash_str(6, "title".to_string());
        // create the window:
        let mut window = Default::default();
        nwg::Window::builder()
            .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
            .position((x, y))
            //.size((w, h))
            .title(&title)
            .build(&mut window)
            .unwrap();
        /*
        let mut canvas = Default::default();
        nwg::ExternCanvas::builder()
            .position((0, 0))
            .size((w, 10))
            .parent(Some(&window))
            .build(&mut canvas)
            .unwrap(); */
        info!("new-window {:?} : {:?}", wid, title);
        if let nwg::ControlHandle::Hwnd(handle) = window.handle {
            let hwnd = handle;
            let window_handle = window.handle;
            let window = Rc::new(window);

            let client_wrapper = self.lock.clone().expect("no client!");
            let handler = nwg::full_bind_event_handler(&window_handle, move |evt, evt_data, handle| {
                info!("event {:?} wid={:?} window_handle={:?} handle={:?}", evt, wid, window_handle, handle);
                let mut xc = client_wrapper.lock().unwrap();
                xc.handle_window_event(wid, evt, &evt_data, handle);
            });
            // create the model for this window:
            let xpra_window = XpraWindow {
                wid: wid,
                window: window,
                hwnd: hwnd,
                handler: handler,
                mapped: false,
                hdc: None,
                bitmap: None,
                width: w,
                height: h,
                paint_debug: true,
            };
            self.windows.insert(wid, xpra_window);
        }
        else {
            error!("handle does not match!?");
        }
    }

    fn process_lost_window(&mut self, packet: &Packet) {
        let wid = packet.get_i64(1);
        if self.windows.remove(&wid).is_none() {
            warn!("window {:?} not found!", wid);
        }
    }

    fn process_window_metadata(&mut self, packet: &Packet) {
        let wid = packet.get_i64(1);
        let metadata = &packet.main[2];
        info!("window-metadata for {:?}: {:?}", wid, metadata);
    }

    fn process_decoded_draw(&mut self, packet: &mut Packet) {
        let p = packet;
        let wid = p.get_i64(1);
        let x = p.get_i32(2);
        let y = p.get_i32(3);
        let w = p.get_i32(4);
        let h = p.get_i32(5);
        let coding = p.get_str(6);
        let pixels = p.get_bytes(7);
        let seq = p.get_i64(8);
         //let options = yaml_dict...

        let wres = self.windows.get(&wid);
        if wres.is_none() {
            let message = "window not found!".to_string();
            self.send_damage_sequence(seq, wid, w, h, -1, message);
            return;
        }
        let window = wres.unwrap();
        info!("draw {:?} : {:?}", wid, coding);
        let start = SystemTime::now();
        let end = SystemTime::now();
        let decode_time: i128 = end.duration_since(start).unwrap().as_millis() as i128;

        window.paint(seq, x, y, w, h, &coding, &pixels);

        // send ack:
        let message = "".to_string();
        self.send_damage_sequence(seq, wid, w, h, decode_time, message);
    }

    pub fn handle_window_event(&mut self, wid: i64, evt: nwg::Event, evt_data: &nwg::EventData, handle: nwg::ControlHandle) -> bool {
        use nwg::Event as E;
        use nwg::MousePressEvent as M;

        match evt {
            E::OnInit => {
                let wres = self.windows.get_mut(&wid);
                if wres.is_none() {
                    warn!("OnInit: window {:?} not found", wid);
                    return false;
                }
                let window = wres.unwrap();
                if window.mapped {
                    debug!("OnInit: window {:?} is already mapped", wid);
                    return true;
                }
                window.mapped = true;
                use std::mem;
                use winapi::um::winuser::{GetWindowRect};
                use winapi::shared::windef::{RECT};
                use nwg::ControlHandle;
                match handle {
                    ControlHandle::Hwnd(hwnd) => {
                        window.new_backing();
                        let x;
                        let y;
                        let w: u32;
                        let h: u32;
                        unsafe {
                            let mut r: RECT = mem::zeroed();
                            GetWindowRect(hwnd, &mut r);
                            x = r.left;
                            y = r.top;
                            w = max(1, r.right - x) as u32;
                            h = max(1, r.bottom - y) as u32;
                        }
                        info!("oninit rect: {:?},{:?},{:?},{:?}", x, y, w, h);
                        self.send_window_map(wid, x, y, w, h);
                        true
                    },
                    _ => {
                        false
                    }
                }
            }
            E::OnPaint => {
                let wres = self.windows.get_mut(&wid);
                if wres.is_none() {
                    debug!("OnPaint: window {:?} not found", wid);
                    return false;
                }
                let window = wres.unwrap();
                if ! window.mapped {
                    debug!("OnPaint: window {:?} is not mapped", wid);
                    return true;
                }
                if let nwg::EventData::OnPaint(paintdata) = evt_data {
                    debug!("OnPaint: {:?}", paintdata);
                    let paintstruct = paintdata.begin_paint();
                    window.draw_screen(paintstruct);
                    paintdata.end_paint(&paintstruct);
                    return true;
                }
                false
            }
            E::OnMouseMove => {
                let (x, y) = nwg::GlobalCursor::position();
                self.send_pointer_position(wid, x, y);
                true
            }
            E::OnMousePress(M::MousePressLeftDown) => {
                let (x, y) = nwg::GlobalCursor::position();
                self.send_pointer_button(wid, 1, true, x, y);
                true
            }
            E::OnMousePress(M::MousePressLeftUp) => {
                let (x, y) = nwg::GlobalCursor::position();
                self.send_pointer_button(wid, 1, false, x, y);
                true
            }
            E::OnKeyPress => {
                if let nwg::EventData::OnKey(keycode) = evt_data {
                    self.send_key_event(wid, keycode, true);
                }
                true
            }
            E::OnKeyRelease => {
                if let nwg::EventData::OnKey(keycode) = evt_data {
                    self.send_key_event(wid, keycode, false);
                }
                true
            }
            E::OnKeyEnter => {
                let keycode = 0x0d;
                self.send_key_event(wid, &keycode, true);
                true
            }
            E::OnWindowClose => {
                // client.send_close();
                self.send_window_close(wid);
                true
            },
            _ => {
                debug!("event {:?} on wid={:?} handle={:?}", evt, wid, handle);
                false
            }
        }
    }
}
