use log::{debug, trace};

pub fn decode(coding: &String, data: Vec<u8>) -> Result<Vec<u8>, String> {
    debug!("decode {:?}: {:?} bytes", coding, data.len());
    trace!("data={:?}", data);
    if coding == "jpeg" {
        use turbojpeg::{Decompressor, Image, PixelFormat};
        let mut decompressor =
            Decompressor::new().map_err(|e| format!("jpeg decoder initialization failed: {e}"))?;

        let header = decompressor
            .read_header(&data)
            .map_err(|e| format!("invalid jpeg header: {e}"))?;
        let (width, height) = (header.width, header.height);
        let mut image = Image {
            pixels: vec![0; 4 * width * height],
            width,
            pitch: 4 * width,
            height,
            format: PixelFormat::BGRA,
        };
        decompressor
            .decompress(&data, image.as_deref_mut())
            .map_err(|e| format!("jpeg decoding failed: {e}"))?;
        return Ok(image.pixels);
    }
    if coding == "png" {
        use spng;
        let out_format = spng::Format::Rgba8;
        let mut ctx = spng::raw::RawContext::new()
            .map_err(|e| format!("png decoder initialization failed: {e:?}"))?;
        ctx.set_png_buffer(&data)
            .map_err(|e| format!("invalid png: {e:?}"))?;
        let size = ctx
            .decoded_image_size(out_format)
            .map_err(|e| format!("invalid png image size: {e:?}"))?;
        let mut data: Vec<u8> = vec![0; size];
        ctx.decode_image(&mut data, out_format, spng::DecodeFlags::empty())
            .map_err(|e| format!("png decoding failed: {e:?}"))?;
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
// which arrive on the UI thread (unlike draws, which run on the decode thread). Every fallible
// spng call is turned into an error so malformed image data cannot abort the process.
pub fn decode_png_rgba(data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let out_format = spng::Format::Rgba8;
    let mut ctx = spng::raw::RawContext::new().map_err(|e| format!("spng init failed: {e:?}"))?;
    ctx.set_png_buffer(data)
        .map_err(|e| format!("invalid png: {e:?}"))?;
    let ihdr = ctx
        .get_ihdr()
        .map_err(|e| format!("invalid png header: {e:?}"))?;
    let size = ctx
        .decoded_image_size(out_format)
        .map_err(|e| format!("bad png size: {e:?}"))?;
    let mut pixels: Vec<u8> = vec![0; size];
    ctx.decode_image(&mut pixels, out_format, spng::DecodeFlags::empty())
        .map_err(|e| format!("png decode failed: {e:?}"))?;
    Ok((ihdr.width, ihdr.height, pixels))
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn malformed_jpeg_returns_an_error() {
        let error = decode(&"jpeg".to_string(), b"not a jpeg".to_vec()).unwrap_err();
        assert!(error.contains("jpeg"), "unexpected error: {error}");
    }

    #[test]
    fn malformed_png_returns_an_error() {
        let error = decode(&"png".to_string(), b"not a png".to_vec()).unwrap_err();
        assert!(error.contains("png"), "unexpected error: {error}");
    }
}
