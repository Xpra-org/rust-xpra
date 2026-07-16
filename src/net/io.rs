use std::io::{Read, Error, ErrorKind};
use std::result::{Result};
use log::{trace};

use super::connection::Connection;

// The compression algorithm is carried in the high bits of the header's "level" byte (xpra
// net/protocol/header.py): 0x10 = lz4, 0x40 = brotli, 0x80 = zstd (the low nibble is the level).
// We advertise only lz4 (see the client's send_hello), so that is the only one we accept here.
const LZ4_FLAG: u8 = 0x10;


pub fn read_packet(stream: &mut Connection) -> Result<Vec<u8>, Error> {
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
    let compression = header[2];
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
    if compression != 0 {
        payload = decompress(compression, &payload)?;
    }
    return Ok(payload);
}


// Undo the packet compression signalled by the header's "level" byte. Only lz4 is supported (the
// only compressor we advertise); anything else is a protocol violation on our part and errors.
fn decompress(compression: u8, payload: &[u8]) -> Result<Vec<u8>, Error> {
    if compression & LZ4_FLAG == 0 {
        return Err(Error::new(ErrorKind::InvalidData, format!("unsupported compression flag: {:#x}", compression)));
    }
    // xpra frames lz4 as a 4-byte little-endian uncompressed-size prefix followed by a raw lz4
    // block - exactly lz4_flex's size-prepended block format.
    lz4_flex::block::decompress_size_prepended(payload)
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("lz4 decompression failed: {}", e)))
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
    buf
}


pub fn write_packet(stream: &mut Connection, data: &[u8]) -> Result<(), Error> {
    let mut packet = make_header(data);
    packet.extend_from_slice(data);
    stream.write_all(&packet)
}
