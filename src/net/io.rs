use std::io::{Read, Error, Write, ErrorKind};
use log::{trace};
use std::result::{Result};
use std::net::{TcpStream};


pub fn read_packet(mut stream: &TcpStream) -> Result<Vec<u8>, Error> {
    let mut header = [0; 8];
    stream.read_exact(&mut header)?;
    trace!("read_packet header={:?}", header);
    // parse header:
    if header[0] != 0x50 {  // "P"
        return Err(Error::new(ErrorKind::InvalidData, format!("invalid packet header byte: {:?}", header[0])));
    }
    if header[3] != 0 {     // no chunks
        return Err(Error::new(ErrorKind::InvalidData, "chunking is not implemented yet!"));
    }
    if header[1] & 0x4 == 0{   // FLAGS_YAML:
        return Err(Error::new(ErrorKind::InvalidData, format!("unsupported packet encoding: {:?}", header[1])));
    }
    if header[2] != 0 {     // no compression
        return Err(Error::new(ErrorKind::InvalidData, format!("unsupported compression: {:?}", header[2])));
    }
    let mut payload_size: usize = 0;
    for i in 0..4 {
        payload_size *= 0x100;
        payload_size += header[i+4] as usize;
    }
    trace!("read_packet payload_size={:?}", payload_size);
    // read payload:
    let mut payload = vec![0u8; payload_size];
    let payload_buf: &mut [u8] = payload.as_mut_slice();
    stream.read_exact(payload_buf)?;
    return Ok(payload);
}


pub fn make_header(data: &[u8]) -> Vec<u8>{
    let mut buf = Vec::<u8>::new();
    buf.push(0x50);     // "P"
    buf.push(0x4);      // FLAGS_YAML
    buf.push(0);        // no compression
    buf.push(0);        // no chunks
    let len = data.len();
    for i in 0..4 {
        let l8 = len >> (8*(3-i));
        buf.push((l8 & 0xff) as u8);
    }
    return buf;
}


pub fn write_packet(mut stream: &TcpStream, data: &[u8]) {
    let header = make_header(&data);
    // debug!("header={:?}", header);
    stream.write_all(&header).expect("write header failed");
    stream.write_all(&data).expect("write packet failed");
    stream.flush().expect("flush failed");
}
