// X11 only: the window-manager hints winit has no API for.
//
// `WM_TRANSIENT_FOR` ties a dialog to the window it belongs to (the WM keeps it above that window,
// centres it there, leaves it out of the taskbar), and `_NET_WM_STATE_MODAL` marks it as blocking
// that window. winit's own `with_parent_window` is not the same thing on X11: it *embeds* the new
// window inside the parent's client area, which is what an X11 "child" window is.
//
// These are plain properties on the window, so any connection to the same display can set them -
// this is a second one, opened on first use, rather than winit's, which it does not expose. It is
// x11rb's pure-Rust connection, so nothing new is linked either.

use log::{debug, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask, PropMode, Window};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

// EWMH `_NET_WM_STATE` client message actions
const NET_WM_STATE_REMOVE: u32 = 0;
const NET_WM_STATE_ADD: u32 = 1;
// source indication: a normal application
const SOURCE_APPLICATION: u32 = 1;

pub struct X11Hints {
    conn: RustConnection,
    root: Window,
    net_wm_state: u32,
    net_wm_state_modal: u32,
}

impl X11Hints {
    pub fn new() -> Option<Self> {
        let (conn, screen) = match x11rb::connect(None) {
            Ok(connection) => connection,
            Err(e) => {
                warn!("cannot connect to the X11 display for window hints: {}", e);
                return None;
            }
        };
        let root = conn.setup().roots.get(screen)?.root;
        let intern = |name: &[u8]| -> Option<u32> {
            Some(conn.intern_atom(false, name).ok()?.reply().ok()?.atom)
        };
        let net_wm_state = intern(b"_NET_WM_STATE")?;
        let net_wm_state_modal = intern(b"_NET_WM_STATE_MODAL")?;
        Some(X11Hints { conn, root, net_wm_state, net_wm_state_modal })
    }

    pub fn set_transient_for(&self, window: Window, owner: Window) {
        debug!("WM_TRANSIENT_FOR({:#x})={:#x}", window, owner);
        let _ = self.conn.change_property32(
            PropMode::REPLACE, window, AtomEnum::WM_TRANSIENT_FOR, AtomEnum::WINDOW, &[owner],
        );
        self.flush();
    }

    // Before the window is mapped, `_NET_WM_STATE` is a property the WM reads when it maps it;
    // afterwards it belongs to the WM and has to be changed through a client message to the root
    // window (EWMH "_NET_WM_STATE").
    pub fn set_modal(&self, window: Window, modal: bool, mapped: bool) {
        debug!("_NET_WM_STATE_MODAL({:#x})={} (mapped={})", window, modal, mapped);
        if mapped {
            let action = if modal { NET_WM_STATE_ADD } else { NET_WM_STATE_REMOVE };
            let event = ClientMessageEvent::new(
                32, window, self.net_wm_state,
                [action, self.net_wm_state_modal, 0, SOURCE_APPLICATION, 0],
            );
            let mask = EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY;
            let _ = self.conn.send_event(false, self.root, mask, event);
        } else if modal {
            let _ = self.conn.change_property32(
                PropMode::APPEND, window, self.net_wm_state, AtomEnum::ATOM, &[self.net_wm_state_modal],
            );
        }
        self.flush();
    }

    fn flush(&self) {
        if let Err(e) = self.conn.flush() {
            warn!("failed to send X11 window hints: {}", e);
        }
    }
}
