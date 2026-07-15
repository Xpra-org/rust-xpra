use log::{trace, debug};


pub fn decode(coding: &String, data: Vec<u8>) -> Result<Vec<u8>, String>{
    debug!("decode {:?}: {:?} bytes", coding, data.len());
    trace!("data={:?}", data);
    if coding == "jpeg" {
        use turbojpeg::{Decompressor, Image, PixelFormat};
        let mut decompressor = Decompressor::new().unwrap();

        let header = decompressor.read_header(&data).unwrap();
        let (width, height) = (header.width, header.height);
        let mut image = Image {
            pixels: vec![0; 4 * width * height],
            width,
            pitch: 4 * width,
            height,
            format: PixelFormat::BGRA,
        };
        decompressor.decompress(&data, image.as_deref_mut()).unwrap();
        return Ok(image.pixels);
    }
    if coding == "png" {
        use spng;
        let out_format = spng::Format::Rgba8;
        let mut ctx = spng::raw::RawContext::new().unwrap();
        ctx.set_png_buffer(&data).unwrap();
        let size = ctx.decoded_image_size(out_format).unwrap();
        let mut data: Vec<u8> = vec![0; size];
        ctx.decode_image(&mut data, out_format, spng::DecodeFlags::empty()).unwrap();
        return Ok(data);
    }
    if coding == "webp" {
        use libwebp_sys::{WebPDecodeBGRA, WebPFree};
        let (mut width, mut height) = (0i32, 0i32);
        // WebPDecodeBGRA allocates the output itself and hands us the pointer, so we have to copy
        // it into a Vec and free it - it also fills in the dimensions, which we can then validate
        // against the ones the server told us to expect.
        let (pixels, len) = unsafe {
            let ptr = WebPDecodeBGRA(data.as_ptr(), data.len(), &mut width, &mut height);
            if ptr.is_null() {
                return Err("webp decoding failed".to_string());
            }
            let len = (width as usize) * (height as usize) * 4;
            let pixels = std::slice::from_raw_parts(ptr, len).to_vec();
            WebPFree(ptr as *mut std::ffi::c_void);
            (pixels, len)
        };
        if len == 0 {
            return Err(format!("invalid webp image size {width}x{height}"));
        }
        return Ok(pixels);
    }
    Err(format!("unsupported encoding {coding}"))
}


// Decode a png into (width, height, RGBA8) with proper error handling - used for window icons,
// which arrive on the UI thread (unlike draws, which run on the decode thread). The draw path
// above unwraps freely, which is fine there but would abort the whole process (panic = "abort")
// on a malformed icon; here every fallible spng call is turned into an error the caller logs.
pub fn decode_png_rgba(data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let out_format = spng::Format::Rgba8;
    let mut ctx = spng::raw::RawContext::new().map_err(|e| format!("spng init failed: {e:?}"))?;
    ctx.set_png_buffer(data).map_err(|e| format!("invalid png: {e:?}"))?;
    let ihdr = ctx.get_ihdr().map_err(|e| format!("invalid png header: {e:?}"))?;
    let size = ctx.decoded_image_size(out_format).map_err(|e| format!("bad png size: {e:?}"))?;
    let mut pixels: Vec<u8> = vec![0; size];
    ctx.decode_image(&mut pixels, out_format, spng::DecodeFlags::empty())
        .map_err(|e| format!("png decode failed: {e:?}"))?;
    Ok((ihdr.width, ihdr.height, pixels))
}
