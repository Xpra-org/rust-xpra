
extern crate native_windows_gui as nwg;

use std::rc::Rc;
use std::mem::size_of;
use log::{debug, error};
use winapi::shared::windef::{HDC, HBITMAP, RECT, HWND};
use winapi::shared::ntdef::LONG;
use winapi::shared::minwindef::{DWORD};
use winapi::um::wingdi::{
    CreateCompatibleDC, CreateCompatibleBitmap,
    DeleteDC, DeleteObject,
    SelectObject,
    BitBlt,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, RGBQUAD,
    SetDIBits, DIB_RGB_COLORS, SRCCOPY,
    // for painting the frame:
    CreateSolidBrush, RGB
};
use winapi::um::winuser::{GetDC, ReleaseDC, FrameRect, PAINTSTRUCT};


pub struct XpraWindow {
    pub wid: i64,
    pub window: Rc<nwg::Window>,
    pub hwnd: HWND,
    // pub canvas: nwg::ExternCanvas,
    pub handler: nwg::EventHandler,
    pub width: i32,
    pub height: i32,
    pub mapped: bool,
    pub hdc: Option<HDC>,
    pub bitmap: Option<HBITMAP>,
    pub paint_debug: bool,
}


impl XpraWindow {

    pub fn paint(&self, seq: i64, x: i32, y: i32, w: i32, h: i32, coding: &String, pixels: &Vec<u8>) {
        debug!("paint({seq}, {x}, {y}, {w}, {h}, {coding}, {:?} bytes)", pixels.len());
        let hdc = self.hdc.unwrap();
        let bitmap = self.bitmap.unwrap();

        let rgb_size = (w * h * 3) as u32;
        if pixels.len() < rgb_size as usize {
            error!("pixel data is too small!");
            return;
        }

        // create bitmap from the pixel data:
        let header = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as DWORD,
            biWidth: w as LONG,
            biHeight: -h as LONG,
            biPlanes: 1,
            biBitCount: 24,
            biCompression: BI_RGB,
            biSizeImage: rgb_size,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let quad = RGBQUAD { rgbBlue: 0, rgbGreen: 0, rgbRed: 0, rgbReserved: 0};
        let bitmapinfo = BITMAPINFO {
            bmiHeader: header,
            bmiColors: [quad],
        };

        unsafe {
            let window_hdc = hdc;   //GetDC(self.hwnd);
            let update_hdc = CreateCompatibleDC(window_hdc);
            let update_bitmap = CreateCompatibleBitmap(window_hdc, w, h);
            ReleaseDC(self.hwnd, window_hdc);
            if update_bitmap == std::ptr::null_mut() {
                error!("failed to create update bitmap");
                return;
            }
            let data_ptr = pixels.as_ptr();
            debug!("update bitmap {:?} with data at {:?}", update_bitmap, data_ptr);
            let colors = DIB_RGB_COLORS;    //DIB_PAL_COLORS
            if SetDIBits(update_hdc, update_bitmap, 0, h as u32, data_ptr as _, &bitmapinfo, colors) == 0 {
                error!("SetDIBits failed!");
                DeleteObject(update_bitmap as _);
                DeleteDC(update_hdc);
                return;
            }
            SelectObject(update_hdc, update_bitmap as _);
            SelectObject(hdc, bitmap as _);

            let blit = BitBlt(hdc, x, y, w, h, update_hdc, 0, 0, SRCCOPY);
            debug!("blit to offscreen: {:?}", blit);

            // free the temporary bitmap / hdc:
            DeleteObject(update_bitmap as _);
            DeleteDC(update_hdc);

            if self.paint_debug {
                let border = CreateSolidBrush(RGB(255, 0, 0));
                let rect = RECT {left: x, top: y, right: x + w, bottom: y + h};
                FrameRect(hdc, &rect, border as _);
            }
        }
        self.window.invalidate();
    }


    pub fn draw_screen(&self, paintstruct: PAINTSTRUCT) {
        debug!("draw_screen");
        if self.hdc.is_some() {
            unsafe {
                let paint_hdc = paintstruct.hdc;
                let hdc = self.hdc.unwrap();
                debug!("hdc={:?}", hdc);
                SelectObject(hdc, self.bitmap.unwrap() as _);
                let blit = BitBlt(paint_hdc, 0, 0, self.width, self.height, hdc, 0, 0, SRCCOPY);
                debug!("screen blit={:?}", blit);
            }
        }
    }

    pub fn new_backing(&mut self) {
        debug!("new_backing");
        unsafe {
            //let screen_hdc = GetDC(0 as _);
            let window_hdc = GetDC(self.hwnd);
            let dc = CreateCompatibleDC(window_hdc);
            self.hdc = Some(dc);
            let membm = CreateCompatibleBitmap(window_hdc, self.width, self.height);
            if membm != std::ptr::null_mut() {
                self.bitmap = Some(membm);
                debug!("bitmap {:?}", self.bitmap);
            }
            ReleaseDC(self.hwnd, window_hdc);
        }
    }
}

impl Drop for XpraWindow {
    fn drop(&mut self) {
        debug!("Drop XpraWindow {:?}", self.wid);
        if self.bitmap.is_some() {
            let bitmap = self.bitmap.unwrap();
            unsafe {
                // Here is the long winded version of the same code:
                // use winapi::ctypes::c_void;
                // DeleteObject(bitmap as *mut c_void);
                DeleteObject(bitmap as _);
            }
        }
        if self.hdc.is_some() {
            let dc = self.hdc.unwrap();
            unsafe {
                DeleteDC(dc);
            }
        }

        nwg::unbind_event_handler(&self.handler);
        self.window.close();
    }
}
