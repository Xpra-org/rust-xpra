use std::num::NonZeroU32;
use std::rc::Rc;

use log::error;
use softbuffer::{Context, Surface};
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent};
use winit::event_loop::{ActiveEventLoop, OwnedDisplayHandle};
use winit::keyboard::{Key, NamedKey};
use winit::window::Window;

use super::font8x8;

// What a key press did to the dialog, reported back to the event loop.
pub enum DialogAction {
    None,   // still typing, keep the dialog open
    Submit, // Enter: read the password and answer the challenge
    Cancel, // Escape / window closed: give up on authentication
}

// A minimal in-app password prompt, used when no `pinentry` is available (see
// client::process_challenge). It is a plain winit window painted through
// softbuffer - the same stack as XpraWindow - but self-contained: it collects a
// password and reports Submit/Cancel, and never echoes the typed characters
// (only a row of '*'). The challenge parameters live on XpraClient, not here.
pub struct AuthDialog {
    pub window: Rc<Window>,
    surface: Surface<OwnedDisplayHandle, Rc<Window>>,
    framebuffer: Vec<u32>,
    width: u32,
    height: u32,
    prompt: String,
    password: String,
}

const BG: u32 = 0x0020_2020;
const FG: u32 = 0x00E0_E0E0;
const HINT: u32 = 0x0090_9090;

impl AuthDialog {
    pub fn new(
        event_loop: &ActiveEventLoop,
        context: &Context<OwnedDisplayHandle>,
        prompt: String,
    ) -> Result<Self, String> {
        let (width, height) = (460u32, 150u32);
        let attrs = Window::default_attributes()
            .with_title("Xpra Authentication")
            .with_inner_size(PhysicalSize::new(width, height))
            .with_resizable(false);
        let window = event_loop
            .create_window(attrs)
            .map_err(|e| format!("failed to create auth dialog window: {e:?}"))?;
        let window = Rc::new(window);
        let mut surface = Surface::new(context, window.clone())
            .map_err(|e| format!("failed to create auth dialog surface: {e:?}"))?;
        surface
            .resize(NonZeroU32::new(width).unwrap(), NonZeroU32::new(height).unwrap())
            .map_err(|e| format!("failed to size auth dialog surface: {e:?}"))?;
        let mut dialog = AuthDialog {
            window,
            surface,
            framebuffer: vec![BG; (width * height) as usize],
            width,
            height,
            prompt,
            password: String::new(),
        };
        dialog.draw();
        Ok(dialog)
    }

    pub fn draw(&mut self) {
        // the compositor may hand us a different (e.g. HiDPI-scaled) inner size
        // than requested; keep the framebuffer/surface matched to it so present()
        // never has to skip - see window::draw_screen for the same guard.
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
        let fbw = self.width as usize;
        for px in self.framebuffer.iter_mut() {
            *px = BG;
        }
        // prompt text at scale 2, dropping to scale 1 if it wouldn't fit (long usernames), so it
        // is never clipped mid-word:
        let max_w = self.width as i32 - 24;
        let prompt_scale = if self.prompt.len() as i32 * 8 * 2 <= max_w { 2 } else { 1 };
        font8x8::blit_str(&mut self.framebuffer, fbw, 12, 18, prompt_scale, &self.prompt, FG);
        // password field outline + one '*' per typed character (never the text):
        let (fx, fy) = (12i32, 56i32);
        let (fwid, fhei) = (self.width as i32 - 24, 26i32);
        outline(&mut self.framebuffer, fbw, fx, fy, fwid, fhei, FG);
        let mask = "*".repeat(self.password.chars().count());
        font8x8::blit_str(&mut self.framebuffer, fbw, fx + 8, fy + 6, 2, &mask, FG);
        // hint line along the bottom:
        font8x8::blit_str(
            &mut self.framebuffer,
            fbw,
            12,
            self.height as i32 - 20,
            1,
            "Enter = OK     Esc = cancel",
            HINT,
        );
        self.present();
    }

    fn present(&mut self) {
        let mut buffer = match self.surface.buffer_mut() {
            Ok(buffer) => buffer,
            Err(e) => {
                error!("failed to get auth dialog buffer: {:?}", e);
                return;
            }
        };
        if buffer.len() != self.framebuffer.len() {
            return;
        }
        buffer.copy_from_slice(&self.framebuffer);
        if let Err(e) = buffer.present() {
            error!("failed to present auth dialog: {:?}", e);
        }
    }

    pub fn handle_key(&mut self, event: &KeyEvent) -> DialogAction {
        if event.state != ElementState::Pressed {
            return DialogAction::None;
        }
        match &event.logical_key {
            Key::Named(NamedKey::Enter) => return DialogAction::Submit,
            Key::Named(NamedKey::Escape) => return DialogAction::Cancel,
            Key::Named(NamedKey::Backspace) => {
                self.password.pop();
            }
            _ => {
                if let Some(text) = &event.text {
                    for c in text.chars() {
                        if !c.is_control() {
                            self.password.push(c);
                        }
                    }
                }
            }
        }
        self.draw();
        DialogAction::None
    }

    pub fn into_password(self) -> String {
        self.password
    }
}

// Draw a 1px rectangle outline into a `fbw`-wide framebuffer, clipped to it.
fn outline(fb: &mut [u32], fbw: usize, x: i32, y: i32, w: i32, h: i32, color: u32) {
    let fbh = (fb.len() / fbw.max(1)) as i32;
    let mut put = |px: i32, py: i32| {
        if px >= 0 && px < fbw as i32 && py >= 0 && py < fbh {
            fb[py as usize * fbw + px as usize] = color;
        }
    };
    for px in x..x + w {
        put(px, y);
        put(px, y + h - 1);
    }
    for py in y..y + h {
        put(x, py);
        put(x + w - 1, py);
    }
}
