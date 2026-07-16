// Plain-text clipboard access, on a dedicated thread.
//
// `copypasta::ClipboardContext` owns an OS clipboard connection that must stay alive to keep
// serving copied text (on X11 a selection owner has to answer requests for as long as the data
// should remain pasteable), and it is not `Send`, so - exactly like the decode thread's Media
// Foundation objects - it lives entirely on one thread that never hands it out. This thread is the
// only code that touches the OS clipboard; it talks to the UI thread through two channels:
//
//  - **set** (`set_rx`, UI -> here): text the remote end copied, to put on the local clipboard.
//  - **poll** (here -> UI, via the `EventLoopProxy`): every tick we read the local clipboard and,
//    if it changed (a local copy), post a synthesized `clipboard-changed` packet so the UI thread
//    can forward it to the server. This mirrors `start_ping_loop` / the decode thread posting
//    synthesized packets back through the proxy - only the UI thread ever writes to the socket.
//
// `last` guards the copy<->paste feedback loop: a value we just wrote locally (a set) is recorded
// so the very next poll doesn't report it straight back as a fresh local change.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use copypasta::{ClipboardContext, ClipboardProvider};
use log::{debug, info, warn};
use winit::event_loop::EventLoopProxy;

use xpra::net::packet::Packet;
use super::client::client_packet;

// How often we re-read the OS clipboard to notice a local copy. Half a second is responsive enough
// for copy/paste while staying cheap; the clipboard read is a quick round-trip when there is an
// owner (or none), see copypasta's X11 backend.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub fn start_clipboard_loop(proxy: EventLoopProxy<Packet>, set_rx: Receiver<String>) {
    thread::Builder::new().name("clipboard".to_string()).spawn(move || {
        let mut ctx = match ClipboardContext::new() {
            Ok(ctx) => ctx,
            Err(e) => {
                // no usable clipboard (e.g. no X display / XWayland): sync is simply off. The UI
                // thread's sends then go nowhere, which is harmless.
                warn!("clipboard access is unavailable: {}", e);
                return;
            }
        };
        info!("clipboard thread started");
        // seed `last` with whatever is already on the clipboard so we don't forward the
        // pre-existing selection to the server the moment we connect.
        let mut last = ctx.get_contents().unwrap_or_default();
        loop {
            match set_rx.recv_timeout(POLL_INTERVAL) {
                // the UI thread wants text put on the OS clipboard (a paste from the remote):
                Ok(text) => {
                    apply_set(&mut ctx, text, &mut last);
                    // if several arrived while we were busy, keep only the newest:
                    while let Ok(text) = set_rx.try_recv() {
                        apply_set(&mut ctx, text, &mut last);
                    }
                }
                // idle tick: has the local clipboard changed (a local copy)? get_contents errors
                // on a non-utf8 selection (an image, say) - we only sync text, so ignore those.
                Err(RecvTimeoutError::Timeout) => {
                    if let Ok(text) = ctx.get_contents() {
                        if !text.is_empty() && text != last {
                            last = text.clone();
                            if proxy.send_event(client_packet("clipboard-changed", &text)).is_err() {
                                break; // the UI event loop is gone
                            }
                        }
                    }
                }
                // the UI thread dropped its sender: the client is shutting down.
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        debug!("clipboard thread stopping");
    }).unwrap();
}

// Write text to the OS clipboard and remember it as `last`, so the next poll doesn't mistake our
// own write for a local copy and bounce it back to the server.
fn apply_set(ctx: &mut ClipboardContext, text: String, last: &mut String) {
    match ctx.set_contents(text.clone()) {
        Ok(()) => *last = text,
        Err(e) => warn!("failed to set the clipboard: {}", e),
    }
}
