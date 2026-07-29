// The two rectangle primitives the hand-drawn dialogs share (auth_dialog, connect_dialog). There
// is no widget toolkit in this project - windows are server-rendered pixels - so the dialogs are
// blitted by hand into a softbuffer framebuffer, with `font8x8` for the text and these for the
// boxes around it. Coordinates are in framebuffer pixels and everything is clipped to it, so a
// caller may hand over a rectangle that hangs off the edge.

// Fill a rectangle in a `fbw`-wide 0x00RRGGBB framebuffer.
pub fn fill_rect(fb: &mut [u32], fbw: usize, x: i32, y: i32, w: i32, h: i32, color: u32) {
    if fbw == 0 {
        return;
    }
    let fbh = (fb.len() / fbw) as i32;
    for py in y.max(0)..(y + h).min(fbh) {
        let row = py as usize * fbw;
        for px in x.max(0)..(x + w).min(fbw as i32) {
            fb[row + px as usize] = color;
        }
    }
}

// Draw a 1px rectangle outline in a `fbw`-wide 0x00RRGGBB framebuffer.
pub fn outline(fb: &mut [u32], fbw: usize, x: i32, y: i32, w: i32, h: i32, color: u32) {
    if fbw == 0 || w <= 0 || h <= 0 {
        return;
    }
    let fbh = (fb.len() / fbw) as i32;
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
