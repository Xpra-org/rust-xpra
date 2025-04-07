use log::{info, trace};

// use zune_core;
use zune_jpeg;
use zune_png;


pub fn decode(coding: &String, data: Vec<u8>) -> Result<Vec<u8>, String>{
    info!("decode {:?}: {:?} bytes", coding, data.len());
    trace!("data={:?}", data);
    if coding == "jpeg" {
        let mut decoder = zune_jpeg::JpegDecoder::new(&data);
        match decoder.decode() {
            Ok(data) => return Ok(data),
            Err(e) => return Err(format!("jpeg decoding error: {:?}", e))
        };
    }
    else if coding == "png" {
        let mut decoder = zune_png::PngDecoder::new(&data);
        match decoder.decode() {
            Ok(data) => return Ok(data.u8().unwrap()),
            Err(e) => return Err(format!("png decoding error: {:?}", e))
        };
    }
    return Err(format!("unsupported encoding {coding}"));
}
