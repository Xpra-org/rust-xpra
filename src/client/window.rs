use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::Instant;

use log::{debug, error, trace};
use softbuffer::{Context, Surface};
use winit::dpi::PhysicalPosition;
use winit::event_loop::OwnedDisplayHandle;
use winit::window::Window;


pub struct XpraWindow {
    pub wid: u64,
    pub window: Rc<Window>,
    pub surface: Surface<OwnedDisplayHandle, Rc<Window>>,
    pub framebuffer: Vec<u32>,
    pub width: u32,
    pub height: u32,
    pub mapped: bool,
    pub override_redirect: bool,
    // remembered window-level metadata. Updates often contain just one of "above" / "below",
    // so retain both values to derive the effective winit WindowLevel after each partial update.
    pub above: bool,
    pub below: bool,
    pub paint_debug: bool,
    // absolute position of the pointer as of the last CursorMoved event:
    // button and wheel events don't carry a position of their own.
    pub last_cursor: (i32, i32),
}


impl XpraWindow {

    pub fn new(wid: u64, window: Rc<Window>, context: &Context<OwnedDisplayHandle>, width: u32, height: u32, override_redirect: bool) -> Self {
        let mut surface = Surface::new(context, window.clone()).expect("failed to create softbuffer surface");
        let rw = width.max(1);
        let rh = height.max(1);
        surface.resize(NonZeroU32::new(rw).unwrap(), NonZeroU32::new(rh).unwrap())
            .expect("failed to size softbuffer surface");
        XpraWindow {
            wid,
            window,
            surface,
            framebuffer: vec![0u32; (rw * rh) as usize],
            width: rw,
            height: rh,
            mapped: false,
            override_redirect,
            above: false,
            below: false,
            paint_debug: cfg!(debug_assertions),
            last_cursor: (0, 0),
        }
    }

    pub fn paint(&mut self, seq: u64, x: i32, y: i32, w: u32, h: u32, coding: &String, pixels: &Vec<u8>) {
        debug!("paint({seq}, {x}, {y}, {w}, {h}, {coding}, {:?} bytes)", pixels.len());
        let expected = (w as usize) * (h as usize) * 4;
        if pixels.len() < expected {
            error!("pixel data is too small! got {:?} bytes, expected {:?}", pixels.len(), expected);
            return;
        }
        // convert the decoded bytes into softbuffer's 0x00RRGGBB u32 pixels,
        // and composite them into our persistent framebuffer at (x,y):
        let to_pixel: fn(&[u8]) -> u32 = if coding == "jpeg" || coding == "h264" || coding == "webp"
                || coding == "mmap" {
            // turbojpeg outputs BGRA, and so do WebPDecodeBGRA, the Media Foundation h264 path
            // (RGB32) and the shared memory area (we ask the server for BGRX, see send_hello):
            |px: &[u8]| (px[2] as u32) << 16 | (px[1] as u32) << 8 | (px[0] as u32)
        } else {
            // spng outputs RGBA8:
            |px: &[u8]| (px[0] as u32) << 16 | (px[1] as u32) << 8 | (px[2] as u32)
        };
        let t0 = Instant::now();
        for row in 0..h {
            let dst_y = y + row as i32;
            if dst_y < 0 || dst_y as u32 >= self.height {
                continue;
            }
            let src_row_start = (row as usize) * (w as usize) * 4;
            for col in 0..w {
                let dst_x = x + col as i32;
                if dst_x < 0 || dst_x as u32 >= self.width {
                    continue;
                }
                let src_off = src_row_start + (col as usize) * 4;
                let px = to_pixel(&pixels[src_off..src_off + 4]);
                let dst_off = (dst_y as u32) as usize * self.width as usize + dst_x as usize;
                self.framebuffer[dst_off] = px;
            }
        }
        trace!("perf: paint wid={:#x} {:?}x{:?} converted in {:?}", self.wid, w, h, t0.elapsed());
        if self.paint_debug {
            self.draw_debug_border(x, y, w, h);
        }
        self.window.request_redraw();
    }

    fn draw_debug_border(&mut self, x: i32, y: i32, w: u32, h: u32) {
        let color: u32 = 0x00FF0000;
        let x0 = x.max(0) as u32;
        let y0 = y.max(0) as u32;
        let x1 = ((x + w as i32).max(0) as u32).min(self.width);
        let y1 = ((y + h as i32).max(0) as u32).min(self.height);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        for col in x0..x1 {
            self.set_pixel(col, y0, color);
            self.set_pixel(col, y1 - 1, color);
        }
        for row in y0..y1 {
            self.set_pixel(x0, row, color);
            self.set_pixel(x1 - 1, row, color);
        }
    }

    fn set_pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < self.width && y < self.height {
            let off = y as usize * self.width as usize + x as usize;
            self.framebuffer[off] = color;
        }
    }

    pub fn draw_screen(&mut self) {
        trace!("draw_screen wid={:#x}", self.wid);
        let mut buffer = match self.surface.buffer_mut() {
            Ok(buffer) => buffer,
            Err(e) => {
                error!("failed to get softbuffer buffer: {:?}", e);
                return;
            }
        };
        if buffer.len() != self.framebuffer.len() {
            // surface hasn't been resized to match our framebuffer yet, skip this present:
            return;
        }
        let t0 = Instant::now();
        buffer.copy_from_slice(&self.framebuffer);
        let copy_elapsed = t0.elapsed();
        let t1 = Instant::now();
        let result = buffer.present();
        trace!("perf: draw_screen wid={:#x} copy={:?} present={:?}", self.wid, copy_elapsed, t1.elapsed());
        if let Err(e) = result {
            error!("failed to present softbuffer buffer: {:?}", e);
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let rw = width.max(1);
        let rh = height.max(1);
        if rw == self.width && rh == self.height {
            return;
        }
        debug!("resize wid={:#x} to {:?}x{:?}", self.wid, rw, rh);
        if let (Some(w), Some(h)) = (NonZeroU32::new(rw), NonZeroU32::new(rh)) {
            if let Err(e) = self.surface.resize(w, h) {
                error!("failed to resize softbuffer surface: {:?}", e);
                return;
            }
        }
        self.width = rw;
        self.height = rh;
        self.framebuffer = vec![0u32; (rw * rh) as usize];
        self.window.request_redraw();
    }

    pub fn get_geometry(&self) -> (i32, i32, u32, u32) {
        let size = self.window.inner_size();
        let w = size.width.max(1);
        let h = size.height.max(1);
        let pos = self.window.inner_position().unwrap_or(PhysicalPosition::new(0, 0));
        (pos.x, pos.y, w, h)
    }

    // convert a position relative to the client area into the absolute coordinates
    // xpra expects, using the same window origin as get_geometry() (which is what
    // window-map / window-configure told the server) so the two stay consistent -
    // on Wayland both fall back to (0,0) and the server sees window-relative values.
    pub fn absolute_position(&self, position: PhysicalPosition<f64>) -> (i32, i32) {
        let origin = self.window.inner_position().unwrap_or(PhysicalPosition::new(0, 0));
        (origin.x + position.x as i32, origin.y + position.y as i32)
    }

    // convert an inner (client-area) position into the outer position winit's
    // set_outer_position() expects, so we can honour the server's window-move-resize
    // "place the client area at (x,y)" semantics. Not supported on Wayland (returns None).
    pub fn to_outer_position(&self, inner_x: i32, inner_y: i32) -> Option<PhysicalPosition<i32>> {
        let outer = self.window.outer_position().ok()?;
        let inner = self.window.inner_position().ok()?;
        Some(PhysicalPosition::new(inner_x + (outer.x - inner.x), inner_y + (outer.y - inner.y)))
    }
}
