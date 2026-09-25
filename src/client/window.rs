use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::Instant;

use log::{debug, error, trace};
use softbuffer::{Context, Rect, Surface};
use winit::dpi::PhysicalPosition;
use winit::event_loop::OwnedDisplayHandle;
use winit::window::Window;

use super::scaling;


pub struct XpraWindow {
    pub wid: u64,
    pub window: Rc<Window>,
    pub surface: Surface<OwnedDisplayHandle, Rc<Window>>,
    // In *server* pixels (see client/scaling.rs): what the draw packets address.
    pub framebuffer: Vec<u32>,
    pub width: u32,
    pub height: u32,
    // The surface is in physical pixels, `scale` times the framebuffer; presenting scales one
    // into the other through the axis maps (the identity when the factor is 1).
    scale: f64,
    surface_w: u32,
    surface_h: u32,
    xmap: Vec<u32>,
    ymap: Vec<u32>,
    pub mapped: bool,
    pub override_redirect: bool,
    // remembered window-level metadata. Updates often contain just one of "above" / "below",
    // so retain both values to derive the effective winit WindowLevel after each partial update.
    pub above: bool,
    pub below: bool,
    pub paint_debug: bool,
    // absolute position of the pointer as of the last CursorMoved event, in server pixels:
    // button and wheel events don't carry a position of their own.
    pub last_cursor: (i32, i32),
    // Regions of the *surface* to rewrite since the last present, and the damage of the frames
    // before it: softbuffer hands back a recycled buffer holding the pixels we presented `age`
    // frames ago, so a partial copy has to replay the damage of those frames to catch it up.
    dirty: Vec<Rect>,
    history: VecDeque<Vec<Rect>>,
    // Buffer age is only reported by the backends that also take damage rectangles (Wayland, X11
    // with XShm, Win32, Web); elsewhere softbuffer always answers 0, which forces a full copy
    // anyway. Counting the consecutive zeroes gives up on the bookkeeping on such a backend.
    zero_age_streak: u32,
    track_damage: bool,
}


impl XpraWindow {

    // `width`x`height` in server pixels.
    pub fn new(wid: u64, window: Rc<Window>, context: &Context<OwnedDisplayHandle>, width: u32, height: u32, scale: f64, override_redirect: bool) -> Self {
        let mut surface = Surface::new(context, window.clone()).expect("failed to create softbuffer surface");
        let rw = width.max(1);
        let rh = height.max(1);
        let (sw, sh) = (scaling::to_local_size(rw, scale), scaling::to_local_size(rh, scale));
        surface.resize(NonZeroU32::new(sw).unwrap(), NonZeroU32::new(sh).unwrap())
            .expect("failed to size softbuffer surface");
        XpraWindow {
            wid,
            window,
            surface,
            framebuffer: vec![0u32; (rw * rh) as usize],
            width: rw,
            height: rh,
            scale,
            surface_w: sw,
            surface_h: sh,
            xmap: scaling::axis_map(sw, rw, scale),
            ymap: scaling::axis_map(sh, rh, scale),
            mapped: false,
            override_redirect,
            above: false,
            below: false,
            paint_debug: cfg!(debug_assertions),
            last_cursor: (0, 0),
            dirty: Vec::new(),
            history: VecDeque::new(),
            zero_age_streak: 0,
            track_damage: true,
        }
    }

    pub fn scale(&self) -> f64 {
        self.scale
    }

    // Whether the surface shows the framebuffer pixel for pixel, so presenting is a plain copy.
    fn identity(&self) -> bool {
        self.surface_w == self.width && self.surface_h == self.height
    }

    // Record a region of the framebuffer as written - as the surface region that shows it, clipped
    // to both. A rectangle that falls entirely outside is dropped, and a softbuffer Rect cannot
    // be empty.
    fn mark_dirty(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if !self.track_damage {
            return;
        }
        let Some(r) = clip_rect(self.width, self.height, x, y, w, h) else { return };
        let r = if self.identity() { Some(r) } else { scaling::surface_rect(&r, self.scale, self.surface_w, self.surface_h) };
        if let Some(r) = r {
            self.dirty.push(r);
        }
    }

    // Copy one rectangle out of the framebuffer into the surface buffer, row by row.
    fn blit_rect(fb: &[u32], buffer: &mut [u32], stride: u32, r: &Rect) {
        let w = r.width.get() as usize;
        for row in 0..r.height.get() {
            let off = ((r.y + row) * stride + r.x) as usize;
            buffer[off..off + w].copy_from_slice(&fb[off..off + w]);
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
        self.mark_dirty(x, y, w, h);
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
        // take this frame's damage before the surface is borrowed
        let dirty = std::mem::take(&mut self.dirty);
        let identity = self.identity();
        let (sw, sh) = (self.surface_w, self.surface_h);

        let mut buffer = match self.surface.buffer_mut() {
            Ok(buffer) => buffer,
            Err(e) => {
                error!("failed to get softbuffer buffer: {:?}", e);
                self.dirty = dirty;
                return;
            }
        };
        if buffer.len() != (sw * sh) as usize {
            // surface hasn't been resized to match our geometry yet: skip this present, but
            // keep the damage so the next one still paints it.
            self.dirty = dirty;
            return;
        }

        // `age` is how many presents ago this recycled buffer last held our pixels: 0 means its
        // contents are undefined and all of it has to be rewritten, anything else means it is
        // that many frames stale, so replaying the damage of those frames catches it up.
        let age = if self.track_damage { buffer.age() as usize } else { 0 };
        let full = age == 0 || age > self.history.len() + 1 || dirty.is_empty();

        let t0 = Instant::now();
        let damage: Vec<Rect> = if full {
            if identity {
                buffer.copy_from_slice(&self.framebuffer);
            } else if let (Some(w), Some(h)) = (NonZeroU32::new(sw), NonZeroU32::new(sh)) {
                let all = Rect { x: 0, y: 0, width: w, height: h };
                scaling::scale_rect(&self.framebuffer, self.width, &mut buffer, sw, &all, &self.xmap, &self.ymap);
            }
            Vec::new()
        } else {
            let mut rects: Vec<Rect> = dirty.clone();
            for past in self.history.iter().take(age - 1) {
                rects.extend_from_slice(past);
            }
            for r in &rects {
                if identity {
                    Self::blit_rect(&self.framebuffer, &mut buffer, sw, r);
                } else {
                    scaling::scale_rect(&self.framebuffer, self.width, &mut buffer, sw, r, &self.xmap, &self.ymap);
                }
            }
            rects
        };
        let copy_elapsed = t0.elapsed();
        let t1 = Instant::now();
        let rects = damage.len();
        let result = if damage.is_empty() {
            buffer.present()
        } else {
            buffer.present_with_damage(&damage)
        };
        trace!("perf: draw_screen wid={:#x} full={} rects={} copy={:?} present={:?}",
               self.wid, full, rects, copy_elapsed, t1.elapsed());
        if let Err(e) = result {
            error!("failed to present softbuffer buffer: {:?}", e);
        }

        if self.track_damage {
            if age == 0 {
                self.zero_age_streak += 1;
                if self.zero_age_streak >= 8 {
                    debug!("wid={:#x} backend never reports a buffer age, not tracking damage",
                           self.wid);
                    self.track_damage = false;
                    self.dirty = Vec::new();
                    self.history = VecDeque::new();
                    return;
                }
            } else {
                self.zero_age_streak = 0;
            }
            // Remember what this frame wrote so a later partial copy can replay it. A full copy
            // rewrote everything, which is what the next frame has to assume it must replace.
            let written = match (NonZeroU32::new(sw), NonZeroU32::new(sh)) {
                (Some(w), Some(h)) if full => vec![Rect { x: 0, y: 0, width: w, height: h }],
                _ => dirty,
            };
            self.history.push_front(written);
            while self.history.len() > 8 {
                self.history.pop_back();
            }
        }
    }

    // `width`x`height` is the window's new inner size in physical pixels, as winit reports it.
    pub fn resize(&mut self, width: u32, height: u32) {
        let sw = width.max(1);
        let sh = height.max(1);
        if sw == self.surface_w && sh == self.surface_h {
            return;
        }
        let (rw, rh) = (scaling::to_server_size(sw, self.scale), scaling::to_server_size(sh, self.scale));
        debug!("resize wid={:#x} to {:?}x{:?} ({:?}x{:?} on screen)", self.wid, rw, rh, sw, sh);
        if let (Some(w), Some(h)) = (NonZeroU32::new(sw), NonZeroU32::new(sh)) {
            if let Err(e) = self.surface.resize(w, h) {
                error!("failed to resize softbuffer surface: {:?}", e);
                return;
            }
        }
        self.surface_w = sw;
        self.surface_h = sh;
        self.width = rw;
        self.height = rh;
        self.xmap = scaling::axis_map(sw, rw, self.scale);
        self.ymap = scaling::axis_map(sh, rh, self.scale);
        self.framebuffer = vec![0u32; (rw * rh) as usize];
        // the framebuffer was replaced, so every recorded rectangle describes the old geometry:
        // drop them all and let the next present rewrite the whole surface.
        self.dirty.clear();
        self.history.clear();
        self.window.request_redraw();
    }

    // The client area's position and size, in server pixels.
    pub fn get_geometry(&self) -> (i32, i32, u32, u32) {
        let size = self.window.inner_size();
        let pos = self.window.inner_position().unwrap_or(PhysicalPosition::new(0, 0));
        let s = self.scale;
        (scaling::to_server(pos.x, s), scaling::to_server(pos.y, s),
         scaling::to_server_size(size.width, s), scaling::to_server_size(size.height, s))
    }

    // convert a position relative to the client area into the absolute coordinates
    // xpra expects (server pixels), using the same window origin as get_geometry() (which is what
    // window-map / window-configure told the server) so the two stay consistent -
    // on Wayland both fall back to (0,0) and the server sees window-relative values.
    pub fn absolute_position(&self, position: PhysicalPosition<f64>) -> (i32, i32) {
        let origin = self.window.inner_position().unwrap_or(PhysicalPosition::new(0, 0));
        let s = self.scale;
        (((origin.x as f64 + position.x) / s).floor() as i32, ((origin.y as f64 + position.y) / s).floor() as i32)
    }

    // convert an inner (client-area) position, in physical pixels, into the outer position winit's
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


// The part of a `w`x`h` rectangle at (x,y) that lies inside a `fw`x`fh` framebuffer, or None if
// none of it does. Every rectangle handed to `blit_rect` or to `present_with_damage` comes from
// here, so this is what keeps those row slices inside the buffer; a softbuffer Rect also cannot
// be empty, which is the other reason an off-screen rectangle has to become None rather than a
// zero-sized Rect.
fn clip_rect(fw: u32, fh: u32, x: i32, y: i32, w: u32, h: u32) -> Option<Rect> {
    let x0 = x.max(0) as u32;
    let y0 = y.max(0) as u32;
    let x1 = ((x as i64 + w as i64).max(0) as u64).min(fw as u64) as u32;
    let y1 = ((y as i64 + h as i64).max(0) as u64).min(fh as u64) as u32;
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(Rect {
        x: x0,
        y: y0,
        width: NonZeroU32::new(x1 - x0)?,
        height: NonZeroU32::new(y1 - y0)?,
    })
}


#[cfg(test)]
mod tests {
    use super::{blit_into, clip_rect};

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

    // what a clipped rectangle has to satisfy for `blit_rect` to stay inside the framebuffer
    fn assert_inside(fw: u32, fh: u32, x: i32, y: i32, w: u32, h: u32) {
        let Some(r) = clip_rect(fw, fh, x, y, w, h) else { return };
        assert!(r.x + r.width.get() <= fw, "{:?} runs off the right of {fw}", r);
        assert!(r.y + r.height.get() <= fh, "{:?} runs off the bottom of {fh}", r);
    }

    #[test]
    fn a_rectangle_inside_is_unchanged() {
        let r = clip_rect(64, 48, 10, 8, 20, 16).unwrap();
        assert_eq!((r.x, r.y, r.width.get(), r.height.get()), (10, 8, 20, 16));
    }

    #[test]
    fn overhang_is_trimmed() {
        let r = clip_rect(64, 48, -5, -7, 20, 16).unwrap();
        assert_eq!((r.x, r.y, r.width.get(), r.height.get()), (0, 0, 15, 9));
        let r = clip_rect(64, 48, 50, 40, 20, 16).unwrap();
        assert_eq!((r.x, r.y, r.width.get(), r.height.get()), (50, 40, 14, 8));
    }

    #[test]
    fn a_rectangle_fully_outside_is_none() {
        assert!(clip_rect(64, 48, -30, -30, 20, 16).is_none());
        assert!(clip_rect(64, 48, 64, 48, 8, 8).is_none());
        assert!(clip_rect(64, 48, 0, 0, 0, 0).is_none());
    }

    #[test]
    fn the_result_always_fits_the_framebuffer() {
        for &(x, y, w, h) in &[
            (0, 0, 64, 48), (-5, -7, 20, 16), (50, 40, 20, 16), (-3, 20, 70, 4),
            (0, 0, 1, 1), (63, 47, 100, 100), (i32::MIN, 0, 8, 8), (i32::MAX, 0, 8, 8),
            (0, 0, u32::MAX, u32::MAX),
        ] {
            assert_inside(64, 48, x, y, w, h);
        }
    }
}
