// Graceful shutdown on an interrupt: Ctrl-C (and, on Unix, `SIGTERM`/`SIGHUP`) should end the
// session the way the tray's "Exit" item does - telling the server why we are leaving - rather
// than killing the process from under a live connection. On Linux, where there is no tray at all
// (see client/tray.rs), this is the *only* clean way out.
//
// Neither a signal handler nor the Windows console control handler can reach the
// `ActiveEventLoop` that stopping the event loop needs, so - exactly like the tray's window
// procedure - they post a synthesized client-side `interrupt` packet through the
// `EventLoopProxy`, and the UI thread does the rest in `do_process_packet` (or, when the
// connection dialog is still up and there is no session yet, in `App::user_event`).

use winit::event_loop::EventLoopProxy;
use xpra::net::packet::Packet;

// The client-side packet type posted on an interrupt. Field 1 names what was caught, purely so
// that the log line can say so.
pub const INTERRUPT: &str = "interrupt";


#[cfg(unix)]
mod imp {
    use std::ffi::c_int;
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::thread;

    use log::{debug, warn};
    use winit::event_loop::EventLoopProxy;
    use xpra::net::packet::Packet;

    use crate::client::client::client_packet;
    use super::INTERRUPT;

    // The three signals that mean "end the session": Ctrl-C, the polite termination request a
    // service manager sends, and the terminal going away. Everything else is left alone.
    const SIGHUP: c_int = 1;
    const SIGINT: c_int = 2;
    const SIGTERM: c_int = 15;

    // The only libc this module needs, declared rather than depended on - the same approach as
    // client/mmap.rs. `signal`'s handler argument and result are function pointers, taken here as
    // `usize` so that `SIG_DFL` / `SIG_ERR` can be named at all (they are not valid pointers).
    unsafe extern "C" {
        fn pipe(fds: *mut c_int) -> c_int;
        fn read(fd: c_int, buf: *mut u8, count: usize) -> isize;
        fn write(fd: c_int, buf: *const u8, count: usize) -> isize;
        fn signal(signum: c_int, handler: usize) -> usize;
    }

    const SIG_DFL: usize = 0;
    const SIG_ERR: usize = usize::MAX;

    // The write end of the self-pipe, the one thing the handler is allowed to touch.
    static WAKE_FD: AtomicI32 = AtomicI32::new(-1);

    // A signal handler may only call async-signal-safe functions, which rules out
    // `EventLoopProxy::send_event` (it allocates and takes locks) and every logging call. The
    // classic answer is the self-pipe: write one byte - the signal number - and let an ordinary
    // thread turn it into a packet. `signal` and `write` are both on POSIX's async-signal-safe
    // list.
    unsafe extern "C" fn handle_signal(signum: c_int) {
        // put the default handler back, so that a second Ctrl-C kills the process outright: a
        // shutdown stuck on a dead connection must still be interruptible.
        unsafe { signal(signum, SIG_DFL) };
        let fd = WAKE_FD.load(Ordering::Relaxed);
        if fd >= 0 {
            let byte = signum as u8;
            // a failed or short write only means the pipe is full, i.e. an interrupt is already
            // on its way to the UI thread, so there is nothing to report (and no way to report it
            // from here anyway).
            unsafe { write(fd, &byte, 1) };
        }
    }

    fn signal_name(signum: c_int) -> &'static str {
        match signum {
            SIGHUP => "SIGHUP",
            SIGINT => "SIGINT",
            SIGTERM => "SIGTERM",
            _ => "signal",
        }
    }

    pub fn install(proxy: EventLoopProxy<Packet>) {
        let mut fds: [c_int; 2] = [-1, -1];
        if unsafe { pipe(fds.as_mut_ptr()) } != 0 {
            warn!("failed to create the interrupt pipe: interrupts will not be handled");
            return;
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // published before the handlers are installed, so one can never fire on a -1 fd:
        WAKE_FD.store(write_fd, Ordering::Relaxed);
        for signum in [SIGINT, SIGTERM, SIGHUP] {
            if unsafe { signal(signum, handle_signal as *const () as usize) } == SIG_ERR {
                warn!("failed to handle {}", signal_name(signum));
            }
        }
        thread::Builder::new().name("signals".to_string()).spawn(move || {
            let mut attempts = 0;
            let caught = loop {
                let mut byte = 0u8;
                if unsafe { read(read_fd, &mut byte, 1) } == 1 {
                    break byte as c_int;
                }
                // a signal delivered to this very thread interrupts the read (EINTR), which is
                // worth retrying; the count is what stops a descriptor that can never be read
                // from spinning here forever.
                attempts += 1;
                if attempts >= 16 {
                    warn!("giving up reading from the interrupt pipe");
                    return;
                }
            };
            debug!("caught {}", signal_name(caught));
            let _ = proxy.send_event(client_packet(INTERRUPT, signal_name(caught)));
        }).expect("failed to start the signal thread");
    }
}


#[cfg(windows)]
mod imp {
    use std::sync::Mutex;

    use log::{debug, warn};
    use windows::Win32::Foundation::{BOOL, FALSE, TRUE};
    use windows::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT,
        CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };
    use winit::event_loop::EventLoopProxy;
    use xpra::net::packet::Packet;

    use crate::client::client::client_packet;
    use super::INTERRUPT;

    // Unlike a Unix signal handler, the console control handler runs on an ordinary thread the OS
    // injects into the process, so it may do real work - it just has no way of reaching the event
    // loop other than the proxy, which it therefore has to be handed through a static.
    static PROXY: Mutex<Option<EventLoopProxy<Packet>>> = Mutex::new(None);

    unsafe extern "system" fn handle_ctrl(ctrl_type: u32) -> BOOL {
        let name = match ctrl_type {
            CTRL_C_EVENT => "Ctrl-C",
            CTRL_BREAK_EVENT => "Ctrl-Break",
            CTRL_CLOSE_EVENT => "console close",
            CTRL_LOGOFF_EVENT => "logoff",
            CTRL_SHUTDOWN_EVENT => "shutdown",
            // not ours: let the next handler (ultimately the default one) deal with it.
            _ => return FALSE,
        };
        debug!("caught {}", name);
        let proxy = PROXY.lock().ok().and_then(|proxy| proxy.clone());
        match proxy {
            // returning TRUE claims the event, which for Ctrl-C/Ctrl-Break is what stops the
            // process being terminated on the spot and lets the UI thread say goodbye first. The
            // close/logoff/shutdown events are terminated after the handler returns whatever it
            // says, so there the goodbye is a race we are simply more likely to win than not.
            Some(proxy) => {
                let _ = proxy.send_event(client_packet(INTERRUPT, name));
                TRUE
            }
            // no event loop to tell: let the default handler kill us.
            None => FALSE,
        }
    }

    pub fn install(proxy: EventLoopProxy<Packet>) {
        match PROXY.lock() {
            Ok(mut slot) => *slot = Some(proxy),
            Err(_) => {
                warn!("failed to store the event loop proxy: interrupts will not be handled");
                return;
            }
        }
        if let Err(e) = unsafe { SetConsoleCtrlHandler(Some(handle_ctrl), TRUE) } {
            warn!("failed to handle console control events: {e}");
        }
    }
}


#[cfg(not(any(unix, windows)))]
mod imp {
    use winit::event_loop::EventLoopProxy;
    use xpra::net::packet::Packet;

    pub fn install(_proxy: EventLoopProxy<Packet>) {}
}


// Arrange for an interrupt to reach the UI thread as an `interrupt` packet. Called once, from
// `main::run`, as soon as there is an event loop proxy to post through; a failure to install is
// logged and otherwise ignored, since it costs nothing but the graceful goodbye.
pub fn install(proxy: EventLoopProxy<Packet>) {
    imp::install(proxy);
}
