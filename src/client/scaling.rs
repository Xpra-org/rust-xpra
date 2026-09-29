// HiDPI: the session runs in *logical* pixels - physical pixels divided by the local display's
// scale factor - and every window is drawn scaled up by that factor.
//
// Without this, a 2x display (a Retina Mac, a 4K laptop panel at 200%) shows every remote window at
// half its intended size: the server lays windows out and renders fonts for the pixel count we
// report, and those pixels are half as big as it assumes. This is what xpra's own client gets from
// GTK, which works in logical ("application") pixels and lets the toolkit scale the backing store:
// it reports its logical desktop and monitor geometry and the server never learns the physical
// size. Doing the same here means the server needs no DPI hint to get sizes right - which it would
// not act on anyway: it applies a client's `dpi` only after it has configured the virtual screen,
// and only through xsettings (xpra server/subsystem/display.py `add_new_client`).
//
// One factor for the whole session, taken from the primary monitor. xpra has one coordinate space
// per session, so a window dragged to a monitor of a different scale cannot be given a different
// one; winit resizes it on the way (ScaleFactorChanged), which we then report like any resize.
//
// `XPRA_DESKTOP_SCALING` overrides it, named after xpra's `--desktop-scaling` option: `off` (or
// `no`, `0`, `1`) turns scaling off - one server pixel per physical pixel, as before - and a number
// forces that factor; `auto`, `on` or no value at all follows the display.

use std::num::NonZeroU32;

use softbuffer::Rect;

pub const ENV: &str = "XPRA_DESKTOP_SCALING";

/// The factor to run the session at, from `XPRA_DESKTOP_SCALING` (if set) and the scale factor
/// the display reports (if any).
pub fn session_scale(setting: Option<&str>, display: Option<f64>) -> f64 {
    let auto = || display.filter(|s| s.is_finite() && *s > 0.0).unwrap_or(1.0);
    let scale = match setting.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None | Some("") | Some("auto") | Some("on") | Some("yes") | Some("true") => auto(),
        Some("off") | Some("no") | Some("false") | Some("0") => 1.0,
        Some(value) => value.trim_end_matches('%').parse::<f64>().ok()
            .map(|v| if value.ends_with('%') { v / 100.0 } else { v })
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or_else(auto),
    };
    // a factor below 1 would make windows *smaller* than the server's pixels; beyond 4 nothing
    // real needs it and the server's virtual screen would get tiny
    scale.clamp(1.0, 4.0)
}

/// A local (physical) coordinate or size in server (logical) pixels.
pub fn to_server(v: i32, scale: f64) -> i32 {
    (v as f64 / scale).round() as i32
}

pub fn to_server_size(v: u32, scale: f64) -> u32 {
    ((v as f64 / scale).round() as u32).max(1)
}

/// A server (logical) coordinate or size in local (physical) pixels.
pub fn to_local(v: i32, scale: f64) -> i32 {
    (v as f64 * scale).round() as i32
}

pub fn to_local_size(v: u32, scale: f64) -> u32 {
    ((v as f64 * scale).round() as u32).max(1)
}

/// For each of `dst` destination pixels along one axis, the source pixel it shows: the one whose
/// scaled-up span covers it. Nearest-neighbour, so at an integer factor every server pixel becomes
/// an exact block - text stays as crisp as the server drew it, only bigger.
pub fn axis_map(dst: u32, src: u32, scale: f64) -> Vec<u32> {
    let last = src.saturating_sub(1);
    (0..dst).map(|i| ((i as f64 / scale) as u32).min(last)).collect()
}

/// The surface rectangle that shows framebuffer rectangle `r`, clipped to the `sw`x`sh` surface:
/// every surface pixel whose source pixel lies in `r`.
pub fn surface_rect(r: &Rect, scale: f64, sw: u32, sh: u32) -> Option<Rect> {
    let x0 = (r.x as f64 * scale).floor() as u32;
    let y0 = (r.y as f64 * scale).floor() as u32;
    let x1 = (((r.x + r.width.get()) as f64 * scale).ceil() as u32).min(sw);
    let y1 = (((r.y + r.height.get()) as f64 * scale).ceil() as u32).min(sh);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(Rect { x: x0, y: y0, width: NonZeroU32::new(x1 - x0)?, height: NonZeroU32::new(y1 - y0)? })
}

/// Fills rectangle `r` of the `sw`-wide surface `dst` from the framebuffer `src` (`fw` wide),
/// sampling through the axis maps.
pub fn scale_rect(src: &[u32], fw: u32, dst: &mut [u32], sw: u32, r: &Rect, xmap: &[u32], ymap: &[u32]) {
    let (x0, x1) = (r.x as usize, (r.x + r.width.get()) as usize);
    let cols = &xmap[x0..x1];
    for sy in r.y..r.y + r.height.get() {
        let src_row = &src[ymap[sy as usize] as usize * fw as usize..][..fw as usize];
        let dst_row = &mut dst[sy as usize * sw as usize + x0..sy as usize * sw as usize + x1];
        for (d, &fx) in dst_row.iter_mut().zip(cols) {
            *d = src_row[fx as usize];
        }
    }
}

/// Scales a cursor image up by `scale` (nearest-neighbour), so the pointer is the size the server
/// meant next to the windows it belongs to. Returns the new size and pixels; the hotspot scales
/// the same way.
pub fn scale_rgba(w: u32, h: u32, rgba: &[u8], scale: f64) -> (u32, u32, Vec<u8>) {
    let (sw, sh) = (to_local_size(w, scale), to_local_size(h, scale));
    let (xmap, ymap) = (axis_map(sw, w, scale), axis_map(sh, h, scale));
    let mut out = Vec::with_capacity((sw * sh * 4) as usize);
    for &fy in &ymap {
        for &fx in &xmap {
            let off = ((fy * w + fx) * 4) as usize;
            out.extend_from_slice(&rgba[off..off + 4]);
        }
    }
    (sw, sh, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_the_display_unless_told_otherwise() {
        assert_eq!(session_scale(None, Some(2.0)), 2.0);
        assert_eq!(session_scale(Some("auto"), Some(1.5)), 1.5);
        assert_eq!(session_scale(None, None), 1.0);
        assert_eq!(session_scale(Some("off"), Some(2.0)), 1.0);
        assert_eq!(session_scale(Some("0"), Some(2.0)), 1.0);
        assert_eq!(session_scale(Some("1.25"), Some(2.0)), 1.25);
        assert_eq!(session_scale(Some("150%"), None), 1.5);
        // nonsense falls back to the display, and the range is bounded
        assert_eq!(session_scale(Some("big"), Some(2.0)), 2.0);
        assert_eq!(session_scale(Some("0.5"), None), 1.0);
        assert_eq!(session_scale(Some("9"), None), 4.0);
        assert_eq!(session_scale(None, Some(f64::NAN)), 1.0);
    }

    #[test]
    fn converts_both_ways() {
        assert_eq!(to_server(3024, 2.0), 1512);
        assert_eq!(to_server(-1512, 2.0), -756);
        assert_eq!(to_local(756, 2.0), 1512);
        assert_eq!(to_server_size(1, 2.0), 1);
        assert_eq!(to_local_size(100, 1.5), 150);
        // identity at 1
        assert_eq!(to_server(1234, 1.0), 1234);
        assert_eq!(to_local_size(77, 1.0), 77);
    }

    #[test]
    fn nearest_neighbour_blocks() {
        assert_eq!(axis_map(6, 3, 2.0), vec![0, 0, 1, 1, 2, 2]);
        assert_eq!(axis_map(3, 2, 1.5), vec![0, 0, 1]);
        // a surface rounded up past the source never samples outside it
        assert_eq!(axis_map(5, 2, 2.0), vec![0, 0, 1, 1, 1]);
    }

    fn rect(x: u32, y: u32, w: u32, h: u32) -> Rect {
        Rect { x, y, width: NonZeroU32::new(w).unwrap(), height: NonZeroU32::new(h).unwrap() }
    }

    #[test]
    fn damage_scales_and_clips() {
        let r = surface_rect(&rect(1, 2, 3, 4), 2.0, 100, 100).unwrap();
        assert_eq!((r.x, r.y, r.width.get(), r.height.get()), (2, 4, 6, 8));
        // fractional factors round outwards so no surface pixel is missed
        let r = surface_rect(&rect(1, 1, 1, 1), 1.5, 100, 100).unwrap();
        assert_eq!((r.x, r.y, r.width.get(), r.height.get()), (1, 1, 2, 2));
        let r = surface_rect(&rect(8, 8, 4, 4), 2.0, 20, 20).unwrap();
        assert_eq!((r.x, r.y, r.width.get(), r.height.get()), (16, 16, 4, 4));
        assert!(surface_rect(&rect(20, 0, 2, 2), 2.0, 20, 20).is_none());
    }

    #[test]
    fn scales_a_rectangle_of_pixels() {
        // 2x2 framebuffer -> 4x4 surface
        let src = [1, 2, 3, 4];
        let mut dst = [0u32; 16];
        let (xmap, ymap) = (axis_map(4, 2, 2.0), axis_map(4, 2, 2.0));
        scale_rect(&src, 2, &mut dst, 4, &rect(0, 0, 4, 4), &xmap, &ymap);
        assert_eq!(dst, [1, 1, 2, 2, 1, 1, 2, 2, 3, 3, 4, 4, 3, 3, 4, 4]);
        // a partial rectangle leaves the rest alone
        let mut dst = [0u32; 16];
        scale_rect(&src, 2, &mut dst, 4, &rect(2, 2, 2, 2), &xmap, &ymap);
        assert_eq!(dst, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 0, 0, 4, 4]);
    }

    #[test]
    fn scales_a_cursor() {
        let rgba = [1, 2, 3, 4, 5, 6, 7, 8];
        let (w, h, out) = scale_rgba(2, 1, &rgba, 2.0);
        assert_eq!((w, h), (4, 2));
        assert_eq!(out, [1, 2, 3, 4, 1, 2, 3, 4, 5, 6, 7, 8, 5, 6, 7, 8, 1, 2, 3, 4, 1, 2, 3, 4, 5, 6, 7, 8, 5, 6, 7, 8]);
    }
}
