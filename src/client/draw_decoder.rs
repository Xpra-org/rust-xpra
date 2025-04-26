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
    Err(format!("unsupported encoding {coding}"))
}
