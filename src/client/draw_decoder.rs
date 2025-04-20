use log::{info, trace};

use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg;
use zune_png;


pub fn decode(coding: &String, data: Vec<u8>) -> Result<Vec<u8>, String>{
    info!("decode {:?}: {:?} bytes", coding, data.len());
    trace!("data={:?}", data);
    if coding == "jpeg" {
        let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::BGRA);
        let mut decoder = zune_jpeg::JpegDecoder::new_with_options(&data, options);
        match decoder.decode() {
            Ok(data) => {
                let info = decoder.info().unwrap();
                info!("size: {:?}x{:?}", info.width, info.height);
                return Ok(data);
            },
            Err(e) => return Err(format!("jpeg decoding error: {:?}", e))
        };
    }
    else if coding == "png" {
        let options = DecoderOptions::default().png_set_add_alpha_channel(true);
        let mut decoder = zune_png::PngDecoder::new_with_options(&data, options);
        match decoder.decode() {
            Ok(data) => return Ok(data.u8().unwrap()),
            Err(e) => return Err(format!("png decoding error: {:?}", e))
        };
    }
    Err(format!("unsupported encoding {coding}"))
}
