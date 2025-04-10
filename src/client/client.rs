
extern crate native_windows_gui as nwg;

use machine_uid;
use std::rc::Rc;
use std::collections::HashMap;
use std::net::{TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime};
use std::sync::mpsc::{Sender};
use std::{thread};

use std::io::{Error, ErrorKind};


//use winapi::um::winuser::DefWindowProcW;
use serde_json::{json, Value};
use yaml_rust2::{Yaml};
use log::{debug, info, warn, error};
use xpra::net::serde::{
    VERSION_KEY_STR,
    yaml_str, yaml_i32, yaml_i64, yaml_hash_str, yaml_bytes,
};

use xpra::VERSION;
use xpra::net::io::{write_packet, read_packet};
use xpra::net::serde::{ parse_payload };
use super::draw_decoder;
use super::window::{XpraWindow};


pub struct XpraClient {
    pub hello_sent: bool,
    pub server_version: String,
    pub windows: HashMap<i64, XpraWindow>,
    pub stream: TcpStream,
    pub lock: Option<Arc<Mutex<XpraClient>>>,
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


    pub fn start_read_loop(&mut self, sender: Sender<Vec<Yaml>>, notice_sender: nwg::NoticeSender) {
        let stream = self.stream.try_clone().unwrap();
        thread::spawn(move || loop {
            loop {
                let payload = read_packet(&stream).unwrap();
                let packet = parse_payload(payload).unwrap();
                // send the packet to the UI thread:
                sender.send(packet).unwrap();
                // notify UI thread:
                notice_sender.notice();
            }
        });
    }

    pub fn process_packet(&mut self, packet: &Vec<Yaml>) -> Result<(), Error> {
        if packet.len() == 0 {
            return Err(Error::new(ErrorKind::InvalidData, "empty packet!"));
        }
        match &packet[0] {
            Yaml::String(packet_type) => {
                self.do_process_packet(packet_type, packet);
            },
                _ => {
                error!("unexpected packet type: {:?}", packet[0]);
                return Err(Error::new(ErrorKind::InvalidData, "packet type is not a String!"));
            }
        }
        return Ok(());
    }

    fn do_process_packet(&mut self, packet_type: &String, packet: &Vec<Yaml>) {
        if packet_type == "hello" {
            assert!(packet.len() > 1);
            self.process_hello(&packet[1]);
        } else if packet_type == "encodings" {
            debug!("got server encodings: {:?}", packet[1]);
        } else if packet_type == "startup-complete" {
            info!("startup complete!");
        } else if packet_type == "new-window" {
            self.process_new_window(packet)
        } else if packet_type == "lost-window" {
            self.process_lost_window(packet)
        } else if packet_type == "window-metadata" {
            self.process_window_metadata(packet)
        } else if packet_type == "draw" {
            self.process_draw(packet)
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

    fn process_new_window(&mut self, packet: &Vec<Yaml>) {
        debug!("new-window {:?}", packet);
        let wid = yaml_i64(&packet[1]);
        let x = yaml_i32(&packet[2]);
        let y = yaml_i32(&packet[3]);
        let w = yaml_i32(&packet[4]);
        let h = yaml_i32(&packet[5]);
        let metadata = &packet[6];
        let title = yaml_hash_str(metadata, "title".to_string());
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
            let xpra_window = XpraWindow {
                wid: wid,
                window: window,
                hwnd: hwnd,
                // canvas: canvas,
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
            //heh!?
        }
    }

    fn process_lost_window(&mut self, packet: &Vec<Yaml>) {
        let wid = yaml_i64(&packet[1]);
        if self.windows.remove(&wid).is_none() {
            warn!("window {:?} not found!", wid);
        }
    }

    fn process_window_metadata(&mut self, packet: &Vec<Yaml>) {
        let wid = yaml_i64(&packet[1]);
        let metadata = &packet[2];
        info!("window-metadata for {:?}: {:?}", wid, metadata);
    }

    fn process_draw(&mut self, packet: &Vec<Yaml>) {
        let wid = yaml_i64(&packet[1]);
        let x = yaml_i32(&packet[2]);
        let y = yaml_i32(&packet[3]);
        let w = yaml_i32(&packet[4]);
        let h = yaml_i32(&packet[5]);
        let coding = yaml_str(&packet[6]);
        let data = yaml_bytes(&packet[7]);
        let seq = yaml_i64(&packet[8]);
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
        let result = draw_decoder::decode(&coding, data);
        if result.is_err() {
            let message = result.unwrap_err();
            self.send_damage_sequence(seq, wid, w, h, -1, message);
            return;
        }
        let pixels = result.unwrap();
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
                // todo: tell the server it is mapped from here instead
                // let packet = json!(["map-window", wid, x, y, w, h, {}, {}]);
                // xc.write_json(packet);
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
                        let w;
                        let h;
                        unsafe {
                            let mut r: RECT = mem::zeroed();
                            GetWindowRect(hwnd, &mut r);
                            x = r.left;
                            y = r.top;
                            w = r.right - x;
                            h = r.bottom - y;
                        }
                        info!("oninit rect: {:?},{:?},{:?},{:?}", x, y, w, h);
                        // self.send_window_map(wid, x, y, w, h);
                        let packet = json!(["map-window", wid, x, y, w, h, {}, {}]);
                        self.write_json(packet);
                        return true;
                    },
                    _ => {
                        return false;
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
                return false;
            }
            E::OnMouseMove => {
                let (x, y) = nwg::GlobalCursor::position();
                self.send_pointer_position(wid, x, y);
                return true;
            }
            E::OnMousePress(M::MousePressLeftDown) => {
                let (x, y) = nwg::GlobalCursor::position();
                self.send_pointer_button(wid, 1, true, x, y);
                return true;
            }
            E::OnMousePress(M::MousePressLeftUp) => {
                let (x, y) = nwg::GlobalCursor::position();
                self.send_pointer_button(wid, 1, false, x, y);
                return true;
            }
            E::OnKeyPress => {
                if let nwg::EventData::OnKey(keycode) = evt_data {
                    self.send_key_event(wid, keycode, true);
                }
                return true;
            }
            E::OnKeyRelease => {
                if let nwg::EventData::OnKey(keycode) = evt_data {
                    self.send_key_event(wid, keycode, false);
                }
                return true;
            }
            E::OnKeyEnter => {
                let keycode = 0x0d;
                self.send_key_event(wid, &keycode, true);
                return true;
            }
            E::OnWindowClose => {
                // client.send_close();
                self.send_window_close(wid);
                return true;
            },
            _ => {
                debug!("event {:?} on wid={:?} handle={:?}", evt, wid, handle);
                return false;
            }
        }
    }
}
