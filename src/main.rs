// #![windows_subsystem = "windows"]

extern crate native_windows_gui as nwg;
extern crate alloc;

use core::ptr;
use std::env;
use std::rc::Rc;
use std::collections::HashMap;
use std::sync::mpsc::{Sender, Receiver, channel};
use std::net::{TcpStream};
use log::{trace, debug, info, error, LevelFilter};
use xpra::net::packet::Packet;
use simple_logger::SimpleLogger;
use winapi::um::winnt::LONG;
use winapi::shared::{ minwindef::DWORD, windef::{HWINEVENTHOOK, HWND} };
use winapi::um::winuser::{EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_SHOW, EVENT_OBJECT_HIDE, EVENT_OBJECT_STATECHANGE, EVENT_OBJECT_VALUECHANGE, EVENT_SYSTEM_FOREGROUND, EVENT_OBJECT_DESTROY, EVENT_OBJECT_REORDER, EVENT_OBJECT_FOCUS, EVENT_OBJECT_CREATE, EVENT_OBJECT_NAMECHANGE};

mod client;
use client::client::{XpraClient, client, XPRA_CLIENT};


fn create_event_window() -> nwg::Window {
    // for now, use a real window:
    let mut window = Default::default();
    nwg::Window::builder()
        .flags(nwg::WindowFlags::WINDOW)
        .size((1, 1))
        .title("Temporary Event Window")
        .build(&mut window)
        .unwrap();
    window
}

fn create_notice(window: &nwg::Window) -> nwg::Notice {
    let mut notice = nwg::Notice::default();
    nwg::Notice::builder()
        .parent(window)
        .build(&mut notice).expect("failed to create notice");
    notice
}


fn main() {
    let level = if cfg!(debug_assertions) {
        LevelFilter::Debug
    }
    else {
        LevelFilter::Info
    };
    SimpleLogger::new().with_level(level).init().unwrap();
    #[allow(deprecated)]
    unsafe {
        nwg::set_dpi_awareness();
    }
    nwg::init().expect("Failed to init Native Windows GUI");
    nwg::Font::set_global_family("Segoe UI").expect("Failed to set default font");

    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        error!("invalid number of arguments: {:?}", args.len());
        error!("usage: {:?} HOST:IP", args[0]);
        return;
    }
    let uri = args[1].clone();
    let stream = TcpStream::connect(uri).expect("connection failed");

    unsafe {
        info!("DPI is {:?}", nwg::dpi());
    }
    // the event window receives MS Windows events,
    // and we also use it to notify the client that packets are available
    let window = create_event_window();
    let window_handle = window.handle;
    let notice = create_notice(&window);
    // this channel is used by I/O threads to send the actual packets to the UI thread:
    let (packet_tx, packet_rx): (Sender<Packet>, Receiver<Packet>) = channel();
    // and this channel is used for sending packets from the I/O thread to the decode thread:
    let (decode_tx, decode_rx): (Sender<Packet>, Receiver<Packet>) = channel();

    let xpra_client = XpraClient {
        hello_sent: false,
        server_version: "".to_string(),
        windows: HashMap::new(),
        stream: stream,
        notice: notice,
        packet_sender: packet_tx,
        decode_sender: decode_tx,
    };
    xpra_client.start_draw_decode_loop(decode_rx);
    xpra_client.register();

    let event_window = Rc::new(window);
    let event_handler_window = event_window.clone();
    let handler = nwg::full_bind_event_handler(&window_handle, move |evt, evt_data, handle| {
        use nwg::Event as E;

        match evt {
            E::OnInit => {
                debug!("OnInit()");
                let client = client();
                if !client.hello_sent {
                    client.start_read_loop();
                    client.hello_sent = true;
                    client.send_hello();
                }
            }
            E::OnWindowClose => {
                debug!("OnWindowClose()");
                if &handle == &event_handler_window as &nwg::Window {
                    nwg::stop_thread_dispatch();
                }
            },
            E::OnNotice => {
                let packet = packet_rx.recv().unwrap();
                trace!("OnNotice packet={:?}", packet.main[0]);
                let boxed = Box::new(packet);
                client().process_packet(boxed).unwrap();
            }
            _ => {
                if !client().handle_window_event(0, evt, &evt_data, handle) {
                    // DefWindowProcW();
                }
            }
        }
    });

    // hook global events:
    use winapi::um::winuser::{EVENT_MAX, EVENT_MIN, SetWinEventHook, UnhookWinEvent};
    // if let nwg::ControlHandle::Hwnd(_handle) = window_handle {
    // let mut process_id = MaybeUninit::uninit();
    // let thread_id = unsafe { GetWindowThreadProcessId(handle, process_id.as_mut_ptr()) };
    // let process_id = unsafe { process_id.assume_init() };
    let hook: HWINEVENTHOOK = unsafe {
        SetWinEventHook(EVENT_MIN, EVENT_MAX, ptr::null_mut(), Some(win_event_hook_callback), 0, 0, 0)
    };

    nwg::dispatch_thread_events();

    nwg::unbind_event_handler(&handler);

    unsafe {
        UnhookWinEvent(hook);
    }
}


extern "system" fn win_event_hook_callback(
    _hook: HWINEVENTHOOK,
    event: DWORD,
    hwnd: HWND,
    id_object: LONG,
    id_child: LONG,
    event_thread: DWORD,
    event_time: DWORD,
) {
    if event == EVENT_OBJECT_LOCATIONCHANGE || event == EVENT_OBJECT_VALUECHANGE
        || event == EVENT_OBJECT_STATECHANGE || event == EVENT_OBJECT_DESTROY
        || event == EVENT_OBJECT_REORDER || event == EVENT_OBJECT_CREATE
        || event == EVENT_OBJECT_NAMECHANGE {
        // silence these
        return;
    }
    if event == EVENT_OBJECT_SHOW || event == EVENT_OBJECT_HIDE {
        // not sure what to do with this
        return;
    }
    if event == EVENT_OBJECT_FOCUS {
        let focus = hwnd;
        debug!("keyboard focus is on {:#x}", focus as u32);
        return;
    }
    if event == EVENT_SYSTEM_FOREGROUND {
        let focus = hwnd;
        debug!("foreground window is {:#x}", focus as u32);
        let client = client();
        let window = client.find_window(focus);
        if window.is_none() {
            debug!("window {:#x} not found", hwnd as u32);
            return;
        }
        if ! window.unwrap().override_redirect {
            client.send_focus(window.unwrap().wid);
        }
        return;
    }
    debug!("event: {:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x}",
            event, hwnd as u32, id_object, id_child, event_thread, event_time);
}
