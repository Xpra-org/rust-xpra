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
        // The byte order is a property of the decoder, so it selects the instantiation once here
        // rather than being re-tested for every pixel: turbojpeg outputs BGRA, and so do
        // WebPDecodeBGRA, the Media Foundation h264 path (RGB32) and the shared memory area (we
        // ask the server for BGRX, see send_hello), whereas spng outputs RGBA8.
        let bgra = coding == "jpeg" || coding == "h264" || coding == "webp" || coding == "mmap";
        let t0 = Instant::now();
        if bgra {
            blit_into::<true>(&mut self.framebuffer, self.width, self.height, x, y, w, h, pixels);
        } else {
            blit_into::<false>(&mut self.framebuffer, self.width, self.height, x, y, w, h, pixels);
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


// Composite a `w`x`h` BGRA/RGBA image into a `fw`x`fh` framebuffer of 0x00RRGGBB pixels at (x,y).
//
// The source rectangle may hang off any edge, so the visible span is worked out once per call
// instead of being re-tested for every pixel, and each row is then a pair of exactly sized
// slices: no bounds check and no indirect call survive the inner loop, which is what lets it
// vectorise. BGRA is a const parameter so each instantiation carries one byte order.
fn blit_into<const BGRA: bool>(fb: &mut [u32], fw: u32, fh: u32,
                               x: i32, y: i32, w: u32, h: u32, pixels: &[u8]) {
    let (xi, yi) = (x as i64, y as i64);
    // the source columns and rows that land inside the framebuffer
    let col0 = (-xi).clamp(0, w as i64);
    let col1 = (fw as i64 - xi).clamp(col0, w as i64);
    let row0 = (-yi).clamp(0, h as i64);
    let row1 = (fh as i64 - yi).clamp(row0, h as i64);
    if col1 <= col0 || row1 <= row0 {
        return;
    }
    let (col0, col1) = (col0 as usize, col1 as usize);
    let count = col1 - col0;
    let stride = fw as usize;
    let src_stride = w as usize * 4;
    let dst_x = (xi + col0 as i64) as usize;
    for row in row0 as usize..row1 as usize {
        let dst_off = ((yi + row as i64) as usize) * stride + dst_x;
        let src_off = row * src_stride + col0 * 4;
        let dst = &mut fb[dst_off..dst_off + count];
        let src = &pixels[src_off..src_off + count * 4];
        for (d, s) in dst.iter_mut().zip(src.chunks_exact(4)) {
            *d = if BGRA {
                (s[2] as u32) << 16 | (s[1] as u32) << 8 | (s[0] as u32)
            } else {
                (s[0] as u32) << 16 | (s[1] as u32) << 8 | (s[2] as u32)
            };
        }
    }
}


#[cfg(test)]
mod tests {
    use super::blit_into;

    // the per-pixel loop this replaced, kept as the reference the fast path has to agree with
    fn reference(fb: &mut [u32], fw: u32, fh: u32,
                 x: i32, y: i32, w: u32, h: u32, pixels: &[u8], bgra: bool) {
        for row in 0..h {
            let dst_y = y + row as i32;
            if dst_y < 0 || dst_y as u32 >= fh {
                continue;
            }
            let src_row_start = (row as usize) * (w as usize) * 4;
            for col in 0..w {
                let dst_x = x + col as i32;
                if dst_x < 0 || dst_x as u32 >= fw {
                    continue;
                }
                let o = src_row_start + (col as usize) * 4;
                let px = if bgra {
                    (pixels[o + 2] as u32) << 16 | (pixels[o + 1] as u32) << 8 | (pixels[o] as u32)
                } else {
                    (pixels[o] as u32) << 16 | (pixels[o + 1] as u32) << 8 | (pixels[o + 2] as u32)
                };
                fb[(dst_y as u32) as usize * fw as usize + dst_x as usize] = px;
            }
        }
    }

    fn check(fw: u32, fh: u32, x: i32, y: i32, w: u32, h: u32, bgra: bool) {
        let pixels: Vec<u8> = (0..(w * h * 4)).map(|i| (i % 251) as u8).collect();
        let mut fast = vec![0u32; (fw * fh) as usize];
        let mut slow = vec![0u32; (fw * fh) as usize];
        if bgra {
            blit_into::<true>(&mut fast, fw, fh, x, y, w, h, &pixels);
        } else {
            blit_into::<false>(&mut fast, fw, fh, x, y, w, h, &pixels);
        }
        reference(&mut slow, fw, fh, x, y, w, h, &pixels, bgra);
        assert_eq!(fast, slow, "mismatch at ({x},{y}) {w}x{h} in {fw}x{fh} bgra={bgra}");
    }

    #[test]
    fn matches_the_per_pixel_reference() {
        for &bgra in &[true, false] {
            check(64, 48, 0, 0, 64, 48, bgra);      // exact fit
            check(64, 48, 10, 8, 20, 16, bgra);     // fully inside
            check(64, 48, -5, -7, 20, 16, bgra);    // clipped at the top left
            check(64, 48, 50, 40, 20, 16, bgra);    // clipped at the bottom right
            check(64, 48, -30, -30, 20, 16, bgra);  // entirely off the top left
            check(64, 48, 64, 48, 8, 8, bgra);      // entirely off the bottom right
            check(64, 48, -3, 20, 70, 4, bgra);     // wider than the framebuffer
            check(64, 48, 0, 0, 1, 1, bgra);        // a single pixel
        }
    }

    #[test]
    fn byte_order_is_0x00rrggbb() {
        let px = [0x11u8, 0x22, 0x33, 0xff];        // b=0x11 g=0x22 r=0x33 read as BGRA
        let mut fb = [0u32; 1];
        blit_into::<true>(&mut fb, 1, 1, 0, 0, 1, 1, &px);
        assert_eq!(fb[0], 0x00332211);
        blit_into::<false>(&mut fb, 1, 1, 0, 0, 1, 1, &px);
        assert_eq!(fb[0], 0x00112233);
    }
}
