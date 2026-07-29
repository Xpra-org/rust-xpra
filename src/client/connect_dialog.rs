// The connection dialog, shown when the client is started with no target on the command line. It
// collects everything `net::uri::parse_target` would otherwise get from argv - protocol, host, port
// and (for ssh) the username - plus an optional password to answer the server's authentication
// challenge with, and hands the lot back as a URI in exactly the form the command line accepts.
//
// Like AuthDialog, and for the same reason (there is no widget toolkit here - windows are
// server-rendered pixels), it is a plain winit window painted through softbuffer, with its text
// blitted from the `font` bitmap font and its boxes from `paint`. winit allows a single event loop per
// process, so this cannot be a throwaway dialog run *before* the client's own loop: main.rs runs it
// as the first state of the application handler and switches to the XpraClient once connected.

use std::num::NonZeroU32;
use std::rc::Rc;

use log::error;
use softbuffer::{Context, Surface};
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent, MouseButton};
use winit::event_loop::{ActiveEventLoop, OwnedDisplayHandle};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::Window;

use super::font;
use super::paint::{fill_rect, outline};

// The transports `parse_target` accepts, each with the port to pre-fill when it is picked: xpra's
// default bind port for the tcp-based ones, and ssh's own well-known port.
const PROTOCOLS: [(&str, u16); 5] = [
    ("tcp", 10000),
    ("ssl", 10000),
    ("ws", 10000),
    ("wss", 10000),
    ("ssh", 22),
];

// What the user did with the dialog, reported back to the application handler in main.rs.
pub enum ConnectAction {
    None,                    // still editing: keep the dialog open
    Cancel,                  // Escape, the Cancel button, or the window closed: give up
    Connect(ConnectDetails), // Connect (or Enter) with a valid host and port
}

// Everything the dialog collects, in the forms the rest of the client already consumes: `uri` is
// what `parse_target` takes on the command line (and what names the session in the Windows tray),
// while the username and password are handed to XpraClient for its `hello` and for answering an
// authentication challenge without prompting a second time.
#[derive(Clone, Debug)]
pub struct ConnectDetails {
    pub uri: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

// The status line under the fields.
enum Status {
    None,
    Error(String),
    // a connection attempt is in flight (it runs on a worker thread - see main.rs), which freezes
    // the fields: only cancelling is still possible.
    Connecting(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Protocol,
    Host,
    Port,
    Username,
    Password,
    Cancel,
    Connect,
}

// Tab order, which is also the top-to-bottom, left-to-right order things are drawn in.
const FOCUS_ORDER: [Focus; 7] = [
    Focus::Protocol,
    Focus::Host,
    Focus::Port,
    Focus::Username,
    Focus::Password,
    Focus::Cancel,
    Focus::Connect,
];

impl Focus {
    // the four editable text fields, as opposed to the drop-down and the two buttons: only these
    // take typed characters (and so only these treat Space as text rather than as "activate").
    fn text_field(self) -> bool {
        matches!(self, Focus::Host | Focus::Port | Focus::Username | Focus::Password)
    }
}

// The dialog is laid out in the units of a 100% display and scaled to physical pixels through
// `px()`, since softbuffer hands us a physical-pixel framebuffer. Only the design *width* is fixed:
// widths and the button row are measured from the actual window size so the layout survives a
// window manager handing us something other than what we asked for. The row heights leave room for
// one line of the 8x16 font (see `font`), which is what everything here is written in.
const DESIGN_W: i32 = 420;
const DESIGN_H: i32 = 270;
const MARGIN: i32 = 16;
const LABEL_X: i32 = 16;
const FIELD_X: i32 = 96; // past the widest label ("Protocol"/"Username"/"Password", 8 characters)
const ROW_Y: i32 = 44;
const ROW_H: i32 = 32;
const FIELD_H: i32 = 24;
const ITEM_H: i32 = 22; // one row of the open drop-down
const BUTTON_W: i32 = 80;
const BUTTON_H: i32 = 26;
const PAD: i32 = 6; // text inset inside a field/button

const BG: u32 = 0x0020_2020;
const FG: u32 = 0x00E0_E0E0;
const HINT: u32 = 0x0090_9090;
const FIELD_BG: u32 = 0x0018_1818;
const BUTTON_BG: u32 = 0x002E_2E2E;
const BORDER: u32 = 0x0050_5050;
const ACCENT: u32 = 0x0059_9EFF;
const ERROR: u32 = 0x00FF_6B6B;

#[derive(Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn contains(&self, (px, py): (i32, i32)) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

pub struct ConnectDialog {
    pub window: Rc<Window>,
    surface: Surface<OwnedDisplayHandle, Rc<Window>>,
    framebuffer: Vec<u32>,
    width: u32,
    height: u32,
    // physical pixels per design unit, refreshed from the window on every draw so a move to a
    // differently-scaled monitor is picked up.
    scale: f64,
    protocol: usize,
    dropdown: bool,
    host: String,
    port: String,
    username: String,
    password: String,
    focus: Focus,
    // shift is tracked here (rather than read from the key event) for Shift+Tab, since winit
    // reports modifier state in its own event.
    shift: bool,
    // the last pointer position, kept because winit's MouseInput carries no coordinates.
    pointer: (i32, i32),
    status: Status,
}

impl ConnectDialog {
    pub fn new(
        event_loop: &ActiveEventLoop,
        context: &Context<OwnedDisplayHandle>,
    ) -> Result<Self, String> {
        // a logical size, unlike AuthDialog's physical one: this window has five rows of text to
        // fit, so it has to come out the same *apparent* size on a HiDPI display.
        let attrs = Window::default_attributes()
            .with_title("Connect to an Xpra server")
            .with_inner_size(LogicalSize::new(DESIGN_W, DESIGN_H))
            .with_resizable(false);
        let window = event_loop
            .create_window(attrs)
            .map_err(|e| format!("failed to create the connection dialog window: {e:?}"))?;
        let window = Rc::new(window);
        let size = window.inner_size();
        let (width, height) = (size.width.max(1), size.height.max(1));
        let mut surface = Surface::new(context, window.clone())
            .map_err(|e| format!("failed to create the connection dialog surface: {e:?}"))?;
        surface
            .resize(NonZeroU32::new(width).unwrap(), NonZeroU32::new(height).unwrap())
            .map_err(|e| format!("failed to size the connection dialog surface: {e:?}"))?;
        let mut dialog = ConnectDialog {
            window,
            surface,
            framebuffer: vec![BG; (width * height) as usize],
            width,
            height,
            scale: 1.0,
            protocol: 0,
            dropdown: false,
            host: String::new(),
            port: PROTOCOLS[0].1.to_string(),
            username: String::new(),
            password: String::new(),
            // the host is the one thing the user must type, so start there.
            focus: Focus::Host,
            shift: false,
            pointer: (-1, -1),
            status: Status::None,
        };
        dialog.draw();
        Ok(dialog)
    }

    // --- geometry (all in physical pixels, so hit-testing and drawing cannot drift apart) -------

    fn px(&self, design: i32) -> i32 {
        (design as f64 * self.scale).round() as i32
    }

    // the font can only be scaled by whole numbers, so it steps where the rest of the layout is
    // continuous: a 150% display gets the same 2x glyphs as a 200% one, in a 1.5x larger window.
    fn font_scale(&self) -> i32 {
        (self.scale.round() as i32).max(1)
    }

    fn glyph_w(&self) -> i32 {
        font::GLYPH_W * self.font_scale()
    }

    fn glyph_h(&self) -> i32 {
        font::GLYPH_H * self.font_scale()
    }

    // the y a single line of text starts at to sit centred in `rect`.
    fn centre_y(&self, rect: &Rect) -> i32 {
        rect.y + (rect.h - self.glyph_h()) / 2
    }

    fn field_rect(&self, row: i32) -> Rect {
        let x = self.px(FIELD_X);
        Rect {
            x,
            y: self.px(ROW_Y + ROW_H * row),
            w: (self.width as i32 - self.px(MARGIN) - x).max(self.px(60)),
            h: self.px(FIELD_H),
        }
    }

    // one row of the open protocol drop-down, drawn as an overlay below the protocol field.
    fn item_rect(&self, index: usize) -> Rect {
        let field = self.field_rect(0);
        Rect {
            x: field.x,
            y: field.y + field.h + self.px(ITEM_H) * index as i32,
            w: field.w,
            h: self.px(ITEM_H),
        }
    }

    // the buttons hang off the bottom right corner of the window, not off the design height.
    fn button_rect(&self, connect: bool) -> Rect {
        let (w, h) = (self.px(BUTTON_W), self.px(BUTTON_H));
        let right = self.width as i32 - self.px(MARGIN);
        Rect {
            x: if connect { right - w } else { right - 2 * w - self.px(12) },
            y: self.height as i32 - self.px(MARGIN) - h,
            w,
            h,
        }
    }

    fn status_y(&self) -> i32 {
        self.button_rect(true).y - self.px(6) - self.glyph_h()
    }

    // --- painting -------------------------------------------------------------------------------

    pub fn draw(&mut self) {
        // the compositor may hand us a different (e.g. HiDPI-scaled) inner size than requested;
        // keep the framebuffer and surface matched to it so present() never has to skip - see
        // window::draw_screen and auth_dialog::draw for the same guard.
        self.scale = self.window.scale_factor().max(0.1);
        let size = self.window.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));
        if w != self.width || h != self.height {
            self.width = w;
            self.height = h;
            self.framebuffer = vec![BG; (w * h) as usize];
            if let (Some(nw), Some(nh)) = (NonZeroU32::new(w), NonZeroU32::new(h)) {
                let _ = self.surface.resize(nw, nh);
            }
        }
        for pixel in self.framebuffer.iter_mut() {
            *pixel = BG;
        }

        self.text(LABEL_X, 14, "Connect to an Xpra server", FG);
        let protocol = PROTOCOLS[self.protocol].0.to_string();
        self.draw_field(0, "Protocol", Focus::Protocol, &protocol, "");
        self.draw_arrow();
        let host = self.host.clone();
        self.draw_field(1, "Host", Focus::Host, &host, "hostname or IP address");
        let port = self.port.clone();
        self.draw_field(2, "Port", Focus::Port, &port, "");
        let username = self.username.clone();
        self.draw_field(3, "Username", Focus::Username, &username, "optional");
        // never echo the password, only its length - like AuthDialog:
        let password = "*".repeat(self.password.chars().count());
        self.draw_field(4, "Password", Focus::Password, &password, "optional");

        self.draw_status();
        self.draw_button(false, "Cancel");
        self.draw_button(true, "Connect");
        // the keyboard hints go beside the buttons, on the free half of the bottom row:
        let hint_y = self.centre_y(&self.button_rect(true));
        self.text_px(self.px(LABEL_X), hint_y, "Enter=connect  Esc=cancel", HINT);
        // the drop-down is an overlay: it is painted last so it covers the rows underneath.
        if self.dropdown {
            self.draw_dropdown();
        }
        self.present();
    }

    fn draw_field(&mut self, row: i32, label: &str, focus: Focus, text: &str, placeholder: &str) {
        let rect = self.field_rect(row);
        let focused = self.focus == focus;
        let text_y = self.centre_y(&rect);
        self.text_px(self.px(LABEL_X), text_y, label, FG);
        fill_rect(&mut self.framebuffer, self.width as usize, rect.x, rect.y, rect.w, rect.h, FIELD_BG);
        let border = if focused { ACCENT } else { BORDER };
        outline(&mut self.framebuffer, self.width as usize, rect.x, rect.y, rect.w, rect.h, border);
        let inner_x = rect.x + self.px(PAD);
        let inner_w = rect.w - 2 * self.px(PAD);
        let glyph_w = self.glyph_w();
        let max_chars = (inner_w / glyph_w).max(1) as usize;
        // the value, or a greyed-out placeholder when there is nothing to show. An over-long value
        // is shown from its *end*, since typing always happens there (this dialog has no caret
        // movement), while a placeholder is cut at the end like any other label.
        let (shown, color) = if text.is_empty() {
            (clip(placeholder, max_chars), HINT)
        } else {
            (tail(text, max_chars), FG)
        };
        self.text_px(inner_x, text_y, &shown, color);
        // a caret block after the text, so it is obvious which field takes what you type. It sits
        // at the start of an empty field, whatever the placeholder in it says.
        if focused && focus.text_field() {
            let typed = if text.is_empty() { 0 } else { shown.chars().count() as i32 };
            let caret_x = (inner_x + typed * glyph_w).min(rect.x + rect.w - self.px(3));
            let (caret_w, caret_h) = (self.px(2).max(1), self.glyph_h());
            let width = self.width as usize;
            fill_rect(&mut self.framebuffer, width, caret_x, text_y, caret_w, caret_h, FG);
        }
    }

    // the little triangle that marks the protocol field as a drop-down.
    fn draw_arrow(&mut self) {
        let rect = self.field_rect(0);
        let size = self.px(9).max(3);
        let cx = rect.x + rect.w - self.px(PAD) - size / 2;
        let top = rect.y + (rect.h - size / 2) / 2;
        let color = if self.dropdown { ACCENT } else { FG };
        for row in 0..size / 2 {
            let half = size / 2 - row;
            fill_rect(&mut self.framebuffer, self.width as usize, cx - half, top + row, 2 * half + 1, 1, color);
        }
    }

    fn draw_dropdown(&mut self) {
        for index in 0..PROTOCOLS.len() {
            let rect = self.item_rect(index);
            let selected = index == self.protocol;
            let bg = if selected { ACCENT } else { FIELD_BG };
            let fg = if selected { BG } else { FG };
            fill_rect(&mut self.framebuffer, self.width as usize, rect.x, rect.y, rect.w, rect.h, bg);
            outline(&mut self.framebuffer, self.width as usize, rect.x, rect.y, rect.w, rect.h, BORDER);
            let (name, port) = PROTOCOLS[index];
            // pad the name so the ports line up under each other:
            let label = format!("{name:<4}  (port {port})");
            let (x, y) = (rect.x + self.px(PAD), self.centre_y(&rect));
            self.text_px(x, y, &label, fg);
        }
    }

    fn draw_button(&mut self, connect: bool, label: &str) {
        let rect = self.button_rect(connect);
        let focused = self.focus == if connect { Focus::Connect } else { Focus::Cancel };
        fill_rect(&mut self.framebuffer, self.width as usize, rect.x, rect.y, rect.w, rect.h, BUTTON_BG);
        let border = if focused { ACCENT } else { BORDER };
        outline(&mut self.framebuffer, self.width as usize, rect.x, rect.y, rect.w, rect.h, border);
        let x = rect.x + (rect.w - font::text_width(label, self.font_scale())) / 2;
        let y = self.centre_y(&rect);
        self.text_px(x, y, label, FG);
    }

    fn draw_status(&mut self) {
        let (message, color) = match &self.status {
            Status::None => return,
            Status::Error(message) => (message.clone(), ERROR),
            Status::Connecting(uri) => (format!("connecting to {uri} ..."), HINT),
        };
        // errors from the network layer are long; clip rather than wrap or spill over the edge.
        let width = self.width as i32 - 2 * self.px(MARGIN);
        let max_chars = (width / self.glyph_w()).max(4) as usize;
        let message = clip(&message, max_chars);
        let y = self.status_y();
        self.text_px(self.px(LABEL_X), y, &message, color);
    }

    // text at a design-unit position (`text_px` takes physical pixels, which is what the layout
    // helpers above return).
    fn text(&mut self, x: i32, y: i32, text: &str, color: u32) {
        let (px, py) = (self.px(x), self.px(y));
        self.text_px(px, py, text, color);
    }

    fn text_px(&mut self, x: i32, y: i32, text: &str, color: u32) {
        let (scale, width) = (self.font_scale(), self.width as usize);
        font::blit_str(&mut self.framebuffer, width, x, y, scale, text, color);
    }

    fn present(&mut self) {
        let mut buffer = match self.surface.buffer_mut() {
            Ok(buffer) => buffer,
            Err(e) => {
                error!("failed to get the connection dialog buffer: {:?}", e);
                return;
            }
        };
        if buffer.len() != self.framebuffer.len() {
            return;
        }
        buffer.copy_from_slice(&self.framebuffer);
        if let Err(e) = buffer.present() {
            error!("failed to present the connection dialog: {:?}", e);
        }
    }

    // --- state reported by main.rs --------------------------------------------------------------

    // show why a connection attempt failed (or why the fields are not usable) and re-enable input.
    pub fn set_error(&mut self, message: String) {
        self.status = Status::Error(message);
        self.draw();
    }

    // freeze the dialog while the connection is being made on a worker thread.
    pub fn set_connecting(&mut self, uri: &str) {
        self.status = Status::Connecting(uri.to_string());
        self.draw();
    }

    fn connecting(&self) -> bool {
        matches!(self.status, Status::Connecting(_))
    }

    // --- input ----------------------------------------------------------------------------------

    pub fn set_modifiers(&mut self, modifiers: ModifiersState) {
        self.shift = modifiers.shift_key();
    }

    pub fn set_pointer(&mut self, position: PhysicalPosition<f64>) {
        self.pointer = (position.x as i32, position.y as i32);
    }

    pub fn handle_mouse(&mut self, state: ElementState, button: MouseButton) -> ConnectAction {
        if state != ElementState::Pressed || button != MouseButton::Left {
            return ConnectAction::None;
        }
        let action = self.click();
        self.draw();
        action
    }

    fn click(&mut self) -> ConnectAction {
        let pointer = self.pointer;
        // while connecting, the only live control is Cancel:
        if self.connecting() {
            return if self.button_rect(false).contains(pointer) {
                ConnectAction::Cancel
            } else {
                ConnectAction::None
            };
        }
        if self.dropdown {
            for index in 0..PROTOCOLS.len() {
                if self.item_rect(index).contains(pointer) {
                    self.set_protocol(index);
                    break;
                }
            }
            // a click anywhere else just closes the drop-down, as a native one would - it does not
            // also activate whatever is underneath.
            self.dropdown = false;
            return ConnectAction::None;
        }
        if self.field_rect(0).contains(pointer) {
            self.focus = Focus::Protocol;
            self.dropdown = true;
            return ConnectAction::None;
        }
        for (row, focus) in [(1, Focus::Host), (2, Focus::Port), (3, Focus::Username), (4, Focus::Password)] {
            if self.field_rect(row).contains(pointer) {
                self.focus = focus;
                return ConnectAction::None;
            }
        }
        if self.button_rect(false).contains(pointer) {
            return ConnectAction::Cancel;
        }
        if self.button_rect(true).contains(pointer) {
            self.focus = Focus::Connect;
            return self.submit();
        }
        ConnectAction::None
    }

    pub fn handle_key(&mut self, event: &KeyEvent) -> ConnectAction {
        if event.state != ElementState::Pressed {
            return ConnectAction::None;
        }
        let action = self.key(event);
        self.draw();
        action
    }

    fn key(&mut self, event: &KeyEvent) -> ConnectAction {
        // the fields are frozen while a connection attempt is in flight, but giving up is not:
        if self.connecting() {
            return match event.logical_key {
                Key::Named(NamedKey::Escape) => ConnectAction::Cancel,
                _ => ConnectAction::None,
            };
        }
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                if self.dropdown {
                    self.dropdown = false;
                    return ConnectAction::None;
                }
                return ConnectAction::Cancel;
            }
            Key::Named(NamedKey::Enter) => {
                // the drop-down selection follows the arrow keys, so Enter only has to close it:
                if self.dropdown {
                    self.dropdown = false;
                    return ConnectAction::None;
                }
                return match self.focus {
                    Focus::Cancel => ConnectAction::Cancel,
                    _ => self.submit(),
                };
            }
            Key::Named(NamedKey::Tab) => {
                self.dropdown = false;
                self.step(if self.shift { -1 } else { 1 });
            }
            // up/down walk the protocol list when that is what is being edited, and the fields
            // otherwise (a plain alternative to Tab):
            Key::Named(NamedKey::ArrowDown) => self.step_or_protocol(1),
            Key::Named(NamedKey::ArrowUp) => self.step_or_protocol(-1),
            Key::Named(NamedKey::Space) if !self.focus.text_field() => {
                return match self.focus {
                    Focus::Protocol => {
                        self.dropdown = !self.dropdown;
                        ConnectAction::None
                    }
                    Focus::Cancel => ConnectAction::Cancel,
                    _ => self.submit(),
                };
            }
            Key::Named(NamedKey::Backspace) => {
                self.status = Status::None;
                if let Some(field) = self.field_mut() {
                    field.pop();
                }
            }
            _ => {
                let Some(text) = &event.text else {
                    return ConnectAction::None;
                };
                // editing anything clears a stale complaint about the previous attempt:
                self.status = Status::None;
                // the port only takes digits, which keeps most typos out of the validation path;
                // the other fields take any printable character (passwords may contain anything).
                let digits_only = self.focus == Focus::Port;
                let limit = if digits_only { 5 } else { 128 };
                let text = text.clone();
                if let Some(field) = self.field_mut() {
                    for c in text.chars() {
                        if c.is_control() || (digits_only && !c.is_ascii_digit()) {
                            continue;
                        }
                        if field.chars().count() < limit {
                            field.push(c);
                        }
                    }
                }
            }
        }
        ConnectAction::None
    }

    fn field_mut(&mut self) -> Option<&mut String> {
        match self.focus {
            Focus::Host => Some(&mut self.host),
            Focus::Port => Some(&mut self.port),
            Focus::Username => Some(&mut self.username),
            Focus::Password => Some(&mut self.password),
            _ => None,
        }
    }

    fn step(&mut self, delta: i32) {
        let count = FOCUS_ORDER.len() as i32;
        let current = FOCUS_ORDER.iter().position(|f| *f == self.focus).unwrap_or(0) as i32;
        self.focus = FOCUS_ORDER[(((current + delta) % count + count) % count) as usize];
    }

    fn step_or_protocol(&mut self, delta: i32) {
        if self.focus != Focus::Protocol && !self.dropdown {
            self.step(delta);
            return;
        }
        let count = PROTOCOLS.len() as i32;
        let index = ((self.protocol as i32 + delta) % count + count) % count;
        self.set_protocol(index as usize);
    }

    fn set_protocol(&mut self, index: usize) {
        if index == self.protocol {
            return;
        }
        self.protocol = index;
        // picking a protocol re-fills the port with that protocol's default: xpra's default bind
        // port for the tcp-based transports, 22 for ssh. This overwrites whatever was in the field,
        // so a port typed for the previous protocol is not silently carried over to the new one.
        self.port = PROTOCOLS[index].1.to_string();
    }

    // Validate the fields and turn them into a connection URI. Anything wrong is reported in the
    // dialog's own status line rather than to the caller, so the user can fix it and retry.
    fn submit(&mut self) -> ConnectAction {
        let host = self.host.trim().to_string();
        if host.is_empty() {
            self.status = Status::Error("please enter a hostname or IP address".to_string());
            self.focus = Focus::Host;
            return ConnectAction::None;
        }
        if host.contains(char::is_whitespace) || host.contains('/') || host.contains('@') {
            self.status = Status::Error("the hostname contains invalid characters".to_string());
            self.focus = Focus::Host;
            return ConnectAction::None;
        }
        let port = match self.port.trim().parse::<u16>() {
            Ok(port) if port > 0 => port,
            _ => {
                self.status = Status::Error("the port must be a number between 1 and 65535".to_string());
                self.focus = Focus::Port;
                return ConnectAction::None;
            }
        };
        let scheme = PROTOCOLS[self.protocol].0;
        let username = non_empty(&self.username);
        self.status = Status::None;
        ConnectAction::Connect(ConnectDetails {
            uri: build_uri(scheme, &host, port, username.as_deref()),
            username,
            password: non_empty(&self.password),
        })
    }
}

// Assemble the validated fields into a URI in the form `net::uri::parse_target` accepts from the
// command line.
fn build_uri(scheme: &str, host: &str, port: u16, username: Option<&str>) -> String {
    // a bare IPv6 address has to be bracketed for `host:port` to be unambiguous (which is what
    // `host_only` and TcpStream::connect expect); do it for the user rather than fail.
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    // ssh is the one scheme `parse_target` reads a `user@` authority from; for the others the
    // username only goes into our `hello` (see client::send_hello).
    match username {
        Some(username) if scheme == "ssh" => format!("{scheme}://{username}@{host}:{port}/"),
        _ => format!("{scheme}://{host}:{port}/"),
    }
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

// The last `max` characters of `text` (all of it when it is short enough).
fn tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    text.chars().skip(count - max).collect()
}

// `text` cut to `max` characters, with an ellipsis marking what was dropped.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max.saturating_sub(3)).collect();
    clipped.push_str("...");
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;
    use xpra::net::uri::{parse_target, Scheme};

    #[test]
    fn dialog_uris_are_parseable_targets() {
        let target = parse_target(&build_uri("tcp", "localhost", 10000, None)).unwrap();
        assert_eq!(target.scheme, Scheme::Tcp);
        assert_eq!(target.address, "localhost:10000");
        assert_eq!(target.username, None);
        // ssh is the only scheme that carries the username in the URI:
        let target = parse_target(&build_uri("ssh", "example.org", 22, Some("user"))).unwrap();
        assert_eq!(target.scheme, Scheme::Ssh);
        assert_eq!(target.address, "example.org:22");
        assert_eq!(target.username.as_deref(), Some("user"));
        assert_eq!(target.path, "");
        // ... the others keep it out of the address, since it would break the connect:
        let target = parse_target(&build_uri("tcp", "example.org", 10000, Some("user"))).unwrap();
        assert_eq!(target.address, "example.org:10000");
        // a bare IPv6 address gets bracketed so that host and port stay separable:
        let target = parse_target(&build_uri("wss", "::1", 443, None)).unwrap();
        assert_eq!(target.scheme, Scheme::WebSocketTls);
        assert_eq!(target.address, "[::1]:443");
        assert_eq!(target.path, "/");
    }

    #[test]
    fn every_protocol_is_a_supported_scheme() {
        for (scheme, port) in PROTOCOLS {
            let target = build_uri(scheme, "localhost", port, None);
            assert!(parse_target(&target).is_ok(), "{target} should be a valid target");
        }
    }

    #[test]
    fn long_values_are_shortened() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("ab", 4), "ab");
        assert_eq!(clip("abcdefgh", 6), "abc...");
        assert_eq!(clip("abc", 6), "abc");
    }
}
