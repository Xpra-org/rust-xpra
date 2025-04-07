// #![windows_subsystem = "windows"]

extern crate native_windows_gui as nwg;
use std::env;
use std::rc::Rc;
use std::collections::HashMap;
use std::sync::mpsc::{Sender, Receiver, channel};
use log::{trace, debug, error};
use std::sync::{Arc, Mutex};
use std::net::{TcpStream};
use std::{thread};
use yaml_rust2::{Yaml};
use simple_logger::SimpleLogger;

use xpra::net::io::{read_packet};
use xpra::net::serde::{ parse_payload };

mod client;


fn start_read_loop(stream: TcpStream, sender: Sender<Vec<Yaml>>, notice_sender: nwg::NoticeSender) {
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


fn create_event_window() -> nwg::Window {
    // for now, use a real window:
    let mut window = Default::default();
    nwg::Window::builder()
        .size((300, 115))
        .position((300, 300))
        .title("Temporary Event Window")
        .build(&mut window)
        .unwrap();
    return window;
}

fn create_notice(window: &nwg::Window) -> nwg::Notice {
    let mut notice = nwg::Notice::default();
    nwg::Notice::builder()
        .parent(window)
        .build(&mut notice).expect("failed to create notice");
    return notice;
}


fn main() {
    SimpleLogger::new().init().unwrap();
    nwg::init().expect("Failed to init Native Windows GUI");
    nwg::Font::set_global_family("Segoe UI").expect("Failed to set default font");

    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        error!("invalid number of arguments: {:?}", args.len());
        error!("usage: {:?} HOST:IP", args[0]);
        return;
    }
    let uri = args[1].clone();
    let wstream = TcpStream::connect(uri).expect("connection failed");
    let rstream = wstream.try_clone().unwrap();

    let xpra_client = client::client::XpraClient {
        hello_sent: false,
        server_version: "".to_string(),
        windows: HashMap::new(),
        stream: wstream,
        lock: None,
    };

    // this is completely overkill
    // because the event handler is single threaded,
    // but the callbacks require some kind of explicit locking:
    let client_wrapper = Arc::new(Mutex::new(xpra_client));
    {
        let mut xc = client_wrapper.lock().unwrap(); 
        xc.lock = Some(client_wrapper.clone());
    }

    let client_clone = client_wrapper.clone();
    let window = create_event_window();
    let window_handle = window.handle;
    let notice = create_notice(&window);
    let notice_sender = notice.sender();
    let (tx, rx): (Sender<Vec<Yaml>>, Receiver<Vec<Yaml>>) = channel();
    let event_window = Rc::new(window);
    let event_handler_window = event_window.clone();
    let handler = nwg::full_bind_event_handler(&window_handle, move |evt, evt_data, handle| {
        use nwg::Event as E;
        debug!("event {:?}", evt);
        let client = client_clone.clone();

        match evt {
            E::OnInit => {
                let mut xc = client.lock().unwrap();
                if ! xc.hello_sent {
                    xc.hello_sent = true;
                    xc.send_hello();
                }
            }
            E::OnWindowClose => {
                if &handle == &event_handler_window as &nwg::Window {
                    nwg::stop_thread_dispatch();
                }
            },
            E::OnNotice => {
                trace!("OnNotice");
                let packet = rx.recv().unwrap();
                let mut client = client.lock().unwrap();
                client.process_packet(&packet).unwrap();
            }
            _ => {
                let mut _client = client.lock().unwrap();
                if ! _client.handle_window_event(0, evt, &evt_data, handle) {
                    // DefWindowProcW();
                }
            }
        }
    });

    start_read_loop(rstream, tx, notice_sender);

    nwg::dispatch_thread_events();

    nwg::unbind_event_handler(&handler);
}
