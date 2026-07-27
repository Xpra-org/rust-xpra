// Windows system tray (notification area) icon, with a right-click menu whose only action is
// "Exit", and the balloon notifications the same icon can raise. Hand-rolled on
// `Shell_NotifyIconW`, in the same spirit as `net/`'s hand-rolled websocket and digests: the API
// surface needed here is small, and the `windows` crate is already a dependency for Media
// Foundation, so this costs a few extra feature flags and no new crate.
//
// **No extra thread.** Windows message delivery is thread-affine, and winit's Windows backend pumps
// every window owned by the thread that called `run_app`: it dispatches with
// `PeekMessageW(&mut msg, 0, ..)` + `DispatchMessageW` (a `0` hwnd filter means "any window of the
// calling thread", and `DispatchMessageW` routes each message to its own class's window procedure -
// ours, not winit's), and it waits in `MsgWaitForMultipleObjectsEx(.., QS_ALLINPUT, ..)`, which
// wakes on posted messages. So the tray window is created on the UI thread in `XpraClient::resumed`
// and winit pumps it for free, under `ControlFlow::Wait`, with no second message loop and no
// cross-thread `HWND`. See winit's `platform_impl/windows/event_loop.rs`.
//
// The one thing a window procedure cannot reach is the `ActiveEventLoop` that `XpraClient::quit`
// needs, so choosing "Exit" posts a synthesized client-side `tray-exit` packet through the
// `EventLoopProxy` - the same trick the ping timer, the decode thread and the pinentry worker use.

use log::{debug, warn};
use winit::event_loop::EventLoopProxy;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_NONE, NIIF_USER, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, GetCursorPos, GetSystemMetrics, GetWindowLongPtrW, LoadIconW, LoadImageW,
    PostMessageW, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetWindowLongPtrW,
    TrackPopupMenu, GWLP_USERDATA, HICON, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTCOLOR, MF_GRAYED,
    MF_SEPARATOR, MF_STRING, SM_CXSMICON, SM_CYSMICON, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP,
    WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_OVERLAPPED,
};

use xpra::net::packet::Packet;

use super::client::client_packet;

// Private message the shell posts to our window for every mouse event on the icon. Anything from
// WM_APP up is an application's to define.
const WM_TRAY: u32 = WM_APP + 1;
// The single tray icon's id, and the single menu command id. Both are ours to pick, but the command
// id must not be 0: `TrackPopupMenu` returns 0 to mean "dismissed without choosing anything".
const TRAY_ICON_ID: u32 = 1;
const IDM_EXIT: usize = 1;
// The icon resource id embedded by build.rs (`set_icon_with_id(.., "1")`).
const ICON_RESOURCE_ID: usize = 1;

// Everything the window procedure needs. Boxed and hung off the window's GWLP_USERDATA, since a
// window procedure is a plain `extern "system" fn` with nowhere else to keep state.
struct TrayState {
    proxy: EventLoopProxy<Packet>,
    // the greyed-out first line of the menu, naming the session ("xpra @ HOST:PORT")
    title: Vec<u16>,
    // "TaskbarCreated", broadcast by the shell when explorer restarts and every tray icon has to be
    // added again. Registered rather than a constant: the id is only assigned at runtime.
    taskbar_created: u32,
    // kept so the icon can be re-added on that broadcast, and deleted on the way out
    nid: NOTIFYICONDATAW,
}

pub struct Tray {
    hwnd: HWND,
    icon: HICON,
    state: *mut TrayState,
    // xpra's notification id (the `nid` field of a `notify_show` packet) for the balloon currently
    // on screen, if any, so that a `notify_close` for a *different* notification doesn't take it
    // down. The shell only ever shows one balloon per icon, so tracking one id is enough.
    shown_notification: Option<u64>,
}

impl Tray {
    // Create the hidden window, load the icon and add it to the notification area. `target` is the
    // connection string as the user typed it, used for the tooltip and the menu header.
    //
    // Must be called on the UI thread (the one running the winit event loop): that is the thread
    // whose message loop pumps the window, and `DestroyWindow` in `Drop` has to happen on the
    // thread that created it.
    pub fn new(proxy: EventLoopProxy<Packet>, target: &str) -> Result<Tray, String> {
        unsafe {
            let hinstance =
                GetModuleHandleW(None).map_err(|e| format!("cannot get the module handle: {e}"))?;

            // Registering the class a second time (if `resumed` ever runs again) fails harmlessly:
            // the class is already there, and `CreateWindowExW` finds it either way.
            let class = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance.into(),
                lpszClassName: w!("XpraTray"),
                ..Default::default()
            };
            RegisterClassW(&class);

            // A normal top-level window that is simply never shown - deliberately *not* a
            // message-only (HWND_MESSAGE) window: those do not receive broadcast messages, and
            // "TaskbarCreated" is broadcast, so a message-only window would silently stop the icon
            // from ever coming back after an explorer restart. WS_EX_TOOLWINDOW keeps it out of the
            // taskbar and the alt-tab list.
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("XpraTray"),
                w!("Xpra"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                hinstance,
                None,
            )
            .map_err(|e| format!("cannot create the tray window: {e}"))?;

            let icon = load_icon();
            let mut nid = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: TRAY_ICON_ID,
                uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
                uCallbackMessage: WM_TRAY,
                hIcon: icon,
                ..Default::default()
            };
            // szTip is a fixed [u16; 128] that has to stay NUL-terminated, hence the truncation.
            write_wide(&mut nid.szTip, &format!("Xpra - {target}"));

            let state = Box::into_raw(Box::new(TrayState {
                proxy,
                title: wide(&format!("xpra @ {target}")),
                taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
                nid,
            }));
            // Nothing is dispatched to us between CreateWindowExW returning and the UI thread going
            // back to winit's message loop, so setting this here (rather than from WM_NCCREATE) is
            // enough: the window procedure never sees a null pointer for a message that matters.
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);

            if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                // clean up rather than leave a stray window and a leaked box behind
                let _ = DestroyWindow(hwnd);
                drop(Box::from_raw(state));
                let _ = DestroyIcon(icon);
                return Err("Shell_NotifyIcon(NIM_ADD) failed".to_string());
            }
            debug!("system tray icon added");
            Ok(Tray { hwnd, icon, state, shown_notification: None })
        }
    }

    // Show a server-forwarded desktop notification as a balloon on the tray icon.
    //
    // `NIM_MODIFY` with `NIF_INFO` is the whole mechanism: `szInfo` is the text, `szInfoTitle` the
    // title. On Windows 10 and later the shell turns these into toast notifications and applies its
    // own Focus-assist / notification-settings policy, so a balloon may legitimately end up in the
    // Action Center instead of on screen - that is the user's setting, not a failure here.
    //
    // Deliberately minimal, matching what the notification-area API can express: no actions (the
    // balloon can be clicked but has no buttons), no hints, and no per-notification icon. The
    // server's `expire_timeout` is ignored too - `NOTIFYICONDATAW`'s `uTimeout` has been ignored
    // since Vista in favour of the system accessibility timeout.
    pub fn show_notification(&mut self, notification_id: u64, app_name: &str, summary: &str, body: &str) {
        // A balloon whose szInfo is empty is not displayed at all (that is how one is taken back -
        // see close_notification), so when there is no body the summary becomes the text and the
        // application name, if the server gave one, becomes the title.
        let (title, text) = if body.is_empty() {
            (if app_name.is_empty() { "Xpra" } else { app_name }, summary)
        } else {
            (if summary.is_empty() { app_name } else { summary }, body)
        };
        if text.is_empty() {
            debug!("notification {notification_id} has no text, not showing a balloon");
            return;
        }
        unsafe {
            // a copy: the stored NOTIFYICONDATAW keeps the icon/message/tooltip flags it was added
            // with, which is what the TaskbarCreated re-add needs.
            let mut nid = (*self.state).nid;
            // NIF_ICON so hIcon is read: NIIF_USER shows the icon's own image (our xpra icon)
            // as the balloon's title icon, rather than the generic "i" NIIF_INFO would give.
            nid.uFlags = NIF_INFO | NIF_ICON;
            nid.dwInfoFlags = NIIF_USER;
            write_wide(&mut nid.szInfo, text);
            write_wide(&mut nid.szInfoTitle, title);
            if Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
                debug!("notification {notification_id} shown as a tray balloon");
                self.shown_notification = Some(notification_id);
            } else {
                warn!("failed to show notification {notification_id} as a tray balloon");
            }
        }
    }

    // The server withdrawing a notification: blank the balloon out again, but only if it is the one
    // we are actually showing. On Windows 10 and later, where balloons are toasts, the shell may
    // well have moved it to the Action Center already and keep it there - there is no notification-
    // area API to recall a toast, so this is best-effort.
    pub fn close_notification(&mut self, notification_id: u64) {
        if self.shown_notification != Some(notification_id) {
            return;
        }
        self.shown_notification = None;
        debug!("withdrawing the tray balloon for notification {notification_id}");
        unsafe {
            let mut nid = (*self.state).nid;
            nid.uFlags = NIF_INFO;
            nid.dwInfoFlags = NIIF_NONE;
            // an empty szInfo is documented as "remove the balloon"
            nid.szInfo[0] = 0;
            nid.szInfoTitle[0] = 0;
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }
}

impl Drop for Tray {
    // Runs on every clean exit: `main::run` keeps the `XpraClient` in a local, so it is dropped when
    // `run` returns - on the UI thread, which is where `DestroyWindow` has to be called. A release
    // build panics with `panic = "abort"` and so skips this, but Windows reclaims the icon when the
    // process dies, so the worst case is a stale icon until the user next hovers over it.
    fn drop(&mut self) {
        unsafe {
            // WM_DESTROY removes the icon too; doing it here first makes it disappear immediately
            // even if the window teardown is delayed, and a second NIM_DELETE is harmless.
            let _ = Shell_NotifyIconW(NIM_DELETE, &(*self.state).nid);
            let _ = DestroyWindow(self.hwnd);
            let _ = DestroyIcon(self.icon);
            drop(Box::from_raw(self.state));
        }
        debug!("system tray icon removed");
    }
}

// Load the icon build.rs embedded into the executable, at the size the notification area actually
// wants. `LoadImageW` at SM_CXSMICON rather than `LoadIconW`, which hands back the large (SM_CXICON)
// icon and leaves the shell to downscale it - visibly soft, and wrong on high DPI.
//
// Falls back to the stock application icon if the resource is missing (a build where the resource
// compiler was unavailable), so the tray still works, just unbranded.
fn load_icon() -> HICON {
    // the integer form of a resource name: the id in the low word, no pointer (MAKEINTRESOURCE).
    let resource = PCWSTR(ICON_RESOURCE_ID as *const u16);
    unsafe {
        if let Ok(hinstance) = GetModuleHandleW(None) {
            match LoadImageW(
                hinstance,
                resource,
                IMAGE_ICON,
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
                LR_DEFAULTCOLOR,
            ) {
                Ok(handle) if !handle.is_invalid() => return HICON(handle.0),
                _ => {
                    warn!("no icon resource in the executable, using the default application icon")
                }
            }
        }
        LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
    }
}

// The tray window's message handler, called by `DispatchMessageW` from winit's own message loop,
// on the UI thread.
//
// Note what is deliberately *not* here: any `PostQuitMessage`. This window shares winit's message
// queue, and a WM_QUIT posted into it would tear the event loop down behind winit's back.
extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState;
        if state.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state = &*state;
        match msg {
            // A mouse event on the icon. We stay on the legacy callback contract (no
            // NIM_SETVERSION / NOTIFYICON_VERSION_4), where lparam is simply the mouse message.
            WM_TRAY => {
                let event = lparam.0 as u32;
                if event == WM_RBUTTONUP || event == WM_LBUTTONUP || event == WM_CONTEXTMENU {
                    show_menu(state, hwnd);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = Shell_NotifyIconW(NIM_DELETE, &state.nid);
                LRESULT(0)
            }
            // explorer.exe restarted and threw away every tray icon: put ours back.
            _ if msg == state.taskbar_created => {
                debug!("the taskbar was recreated, re-adding the tray icon");
                let _ = Shell_NotifyIconW(NIM_ADD, &state.nid);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// Pop the context menu up at the pointer and act on the choice.
//
// `TrackPopupMenu` runs a nested modal message loop, so winit's loop - and therefore painting of the
// remote session - is blocked for as long as the menu is open. That is what any native application
// does, and it only lasts while the user holds the menu open, but it is worth knowing before
// mistaking a briefly frozen session for a bug.
fn show_menu(state: &TrayState, hwnd: HWND) {
    let chosen = unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() {
            return;
        }
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        // a greyed header naming the session, then the only real entry:
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(state.title.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, IDM_EXIT, w!("Exit"));

        // The SetForegroundWindow / PostMessage(WM_NULL) pair around TrackPopupMenu is the
        // documented workaround (Microsoft KB135788) for a menu owned by a window that is not in
        // the foreground: without the first, the menu does not close when the user clicks away;
        // without the second, it can take an extra click to dismiss.
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD,
            pt.x,
            pt.y,
            0,
            hwnd,
            None,
        );
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        chosen
    };

    // TPM_RETURNCMD makes TrackPopupMenu return the chosen command id (0 = dismissed), so there is
    // no WM_COMMAND to handle.
    if chosen.0 as usize == IDM_EXIT {
        // hand it to the UI thread's packet dispatch, which is where the ActiveEventLoop lives.
        let _ = state.proxy.send_event(client_packet("tray-exit", ""));
    }
}

// A NUL-terminated UTF-16 string, for the Win32 calls that take a PCWSTR.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// Copy a string into a fixed-size Win32 wide buffer, truncating so the NUL terminator survives.
fn write_wide(buffer: &mut [u16], s: &str) {
    let encoded: Vec<u16> = s.encode_utf16().take(buffer.len() - 1).collect();
    buffer[..encoded.len()].copy_from_slice(&encoded);
    buffer[encoded.len()] = 0;
}
