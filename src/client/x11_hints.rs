// X11 only: the window-manager hints winit has no API for.
//
// `WM_TRANSIENT_FOR` ties a dialog to the window it belongs to (the WM keeps it above that window,
// centres it there, leaves it out of the taskbar), and `_NET_WM_STATE` carries the EWMH state
// flags (`_NET_WM_STATE_MODAL`, `_NET_WM_STATE_SKIP_TASKBAR`, ...). winit's own
// `with_parent_window` is not the same thing as a transient-for on X11: it *embeds* the new window
// inside the parent's client area, which is what an X11 "child" window is.
//
// These are plain properties on the window, so any connection to the same display can set them -
// this is a second one, opened on first use, rather than winit's, which it does not expose. It is
// x11rb's pure-Rust connection, so nothing new is linked either.
//
// Most hints matter most *before* the window is mapped, since that is when the window manager
// decides how to place and decorate it - some (window type, modal on Mutter) are only read then.
// So a window with any to set is created hidden, gets its `InitialHints`, and is shown afterwards
// (client.rs `process_new_common`); later changes go through the EWMH client messages instead.

use std::cell::RefCell;
use std::collections::HashMap;

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
// xpra's `workspace` values (xpra constants.py)
const WORKSPACE_UNSET: u32 = 65535;
const WORKSPACE_ALL: u32 = 0xffff_ffff;

// What to set on a window before it is mapped.
#[derive(Debug, Default)]
pub struct InitialHints {
    pub transient_for: Option<Window>,
    // `_NET_WM_STATE_*` names, without the prefix: "MODAL", "SKIP_TASKBAR", ...
    pub states: Vec<&'static str>,
    // xpra's `window-type`: `_NET_WM_WINDOW_TYPE_*` names without the prefix ("DIALOG"), or a
    // full atom name for anything outside EWMH ("_KDE_NET_WM_WINDOW_TYPE_OVERRIDE")
    pub window_types: Vec<String>,
    // `_NET_WM_WINDOW_OPACITY`, 0 (transparent) to 0xffffffff (opaque)
    pub opacity: Option<u32>,
    // `_NET_WM_DESKTOP`, already checked by `valid_workspace`
    pub workspace: Option<u32>,
}

impl InitialHints {
    pub fn is_empty(&self) -> bool {
        self.transient_for.is_none() && self.states.is_empty() && self.window_types.is_empty()
            && self.opacity.is_none() && self.workspace.is_none()
    }
}

pub struct X11Hints {
    conn: RustConnection,
    root: Window,
    atoms: RefCell<HashMap<String, u32>>,
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
        Some(X11Hints { conn, root, atoms: RefCell::new(HashMap::new()) })
    }

    fn atom(&self, name: &str) -> Option<u32> {
        if let Some(atom) = self.atoms.borrow().get(name) {
            return Some(*atom);
        }
        let atom = match self.conn.intern_atom(false, name.as_bytes()).map(|cookie| cookie.reply()) {
            Ok(Ok(reply)) => reply.atom,
            _ => {
                warn!("failed to intern X11 atom {}", name);
                return None;
            }
        };
        self.atoms.borrow_mut().insert(name.to_string(), atom);
        Some(atom)
    }

    pub fn apply_initial(&self, window: Window, hints: &InitialHints) {
        debug!("initial X11 hints for {:#x}: {:?}", window, hints);
        if let Some(owner) = hints.transient_for {
            let _ = self.conn.change_property32(
                PropMode::REPLACE, window, AtomEnum::WM_TRANSIENT_FOR, AtomEnum::WINDOW, &[owner],
            );
        }
        if !hints.states.is_empty() {
            let states: Vec<u32> = hints.states.iter()
                .filter_map(|state| self.atom(&format!("_NET_WM_STATE_{}", state)))
                .collect();
            if let Some(net_wm_state) = self.atom("_NET_WM_STATE") {
                let _ = self.conn.change_property32(
                    PropMode::APPEND, window, net_wm_state, AtomEnum::ATOM, &states,
                );
            }
        }
        // winit has already set `_NET_WM_WINDOW_TYPE_NORMAL`: replace it, most specific first
        if !hints.window_types.is_empty() {
            let types: Vec<u32> = hints.window_types.iter()
                .filter_map(|window_type| match window_type.starts_with('_') {
                    true => self.atom(window_type),
                    false => self.atom(&format!("_NET_WM_WINDOW_TYPE_{}", window_type)),
                })
                .collect();
            if let Some(net_wm_window_type) = self.atom("_NET_WM_WINDOW_TYPE") {
                let _ = self.conn.change_property32(
                    PropMode::REPLACE, window, net_wm_window_type, AtomEnum::ATOM, &types,
                );
            }
        }
        if let Some(opacity) = hints.opacity {
            self.set_opacity(window, Some(opacity));
        }
        if let (Some(workspace), Some(atom)) = (hints.workspace, self.atom("_NET_WM_DESKTOP")) {
            let _ = self.conn.change_property32(PropMode::REPLACE, window, atom, AtomEnum::CARDINAL, &[workspace]);
        }
        self.flush();
    }

    // The server's workspace, if it means something here: not "unset", and one of the desktops
    // we actually have - the server mirrors our list, but it may have changed since (xpra
    // client/gtk3/window/workspace.py `set_workspace`).
    pub fn valid_workspace(&self, workspace: u32) -> Option<u32> {
        if workspace == WORKSPACE_UNSET {
            return None;
        }
        if workspace == WORKSPACE_ALL {
            return Some(workspace);
        }
        let count = self.atom("_NET_NUMBER_OF_DESKTOPS")
            .and_then(|atom| self.conn.get_property(false, self.root, atom, AtomEnum::CARDINAL, 0, 1).ok())
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|reply| reply.value32()?.next());
        match count {
            Some(count) if workspace < count => Some(workspace),
            _ => {
                warn!("ignoring invalid workspace {} (the desktop has {:?})", workspace, count);
                None
            }
        }
    }

    // A mapped window is moved by asking the window manager (EWMH "_NET_WM_DESKTOP").
    pub fn set_workspace(&self, window: Window, workspace: u32) {
        debug!("_NET_WM_DESKTOP({:#x})={:#x}", window, workspace);
        if let Some(atom) = self.atom("_NET_WM_DESKTOP") {
            self.send_root_message(window, atom, [workspace, SOURCE_APPLICATION, 0, 0, 0]);
        }
    }

    // Read by the compositor rather than the window manager, and whenever it changes, so this
    // needs no client message once the window is mapped. `None` removes it: fully opaque.
    pub fn set_opacity(&self, window: Window, opacity: Option<u32>) {
        debug!("_NET_WM_WINDOW_OPACITY({:#x})={:x?}", window, opacity);
        let Some(atom) = self.atom("_NET_WM_WINDOW_OPACITY") else {
            return;
        };
        let _ = match opacity {
            Some(opacity) => self.conn.change_property32(PropMode::REPLACE, window, atom, AtomEnum::CARDINAL, &[opacity]),
            None => self.conn.delete_property(window, atom),
        };
        self.flush();
    }

    pub fn set_transient_for(&self, window: Window, owner: Window) {
        debug!("WM_TRANSIENT_FOR({:#x})={:#x}", window, owner);
        let _ = self.conn.change_property32(
            PropMode::REPLACE, window, AtomEnum::WM_TRANSIENT_FOR, AtomEnum::WINDOW, &[owner],
        );
        self.flush();
    }

    // A state change on a mapped window: once mapped, `_NET_WM_STATE` belongs to the window
    // manager and is changed by asking it, with a client message to the root window (EWMH
    // "_NET_WM_STATE"). `state` is the name without its `_NET_WM_STATE_` prefix.
    pub fn set_state(&self, window: Window, state: &str, on: bool) {
        debug!("_NET_WM_STATE_{}({:#x})={}", state, window, on);
        let (Some(net_wm_state), Some(atom)) = (self.atom("_NET_WM_STATE"), self.atom(&format!("_NET_WM_STATE_{}", state))) else {
            return;
        };
        let action = if on { NET_WM_STATE_ADD } else { NET_WM_STATE_REMOVE };
        self.send_root_message(window, net_wm_state, [action, atom, 0, SOURCE_APPLICATION, 0]);
    }

    fn send_root_message(&self, window: Window, message_type: u32, data: [u32; 5]) {
        let event = ClientMessageEvent::new(32, window, message_type, data);
        let mask = EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY;
        let _ = self.conn.send_event(false, self.root, mask, event);
        self.flush();
    }

    fn flush(&self) {
        if let Err(e) = self.conn.flush() {
            warn!("failed to send X11 window hints: {}", e);
        }
    }
}
