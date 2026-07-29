use std::collections::HashMap;
use std::io::{Read, Error, ErrorKind};
use std::result::{Result};
use log::{trace, warn};

use super::connection::Connection;

// The compression algorithm is carried in the high bits of the header's "level" byte (xpra
// net/protocol/header.py): 0x10 = lz4, 0x40 = brotli, 0x80 = zstd (the low nibble is the level).
// We advertise only lz4 (see the client's send_hello), so that is the only one we accept here.
const LZ4_FLAG: u8 = 0x10;
// the header's flags byte: the packet encoder used for the payload. FLAGS_FLUSH (0x8) and
// FLAGS_CIPHER (0x2) can be set alongside it, so this is a mask, not an equality test.
const FLAGS_YAML: u8 = 0x4;

// Out-of-band chunks: rather than inlining a large binary item (pixel data, a window icon, a
// clipboard payload, ...) into the packet's YAML payload as base64, the server can send it as its
// own packet, whose header carries the index of the packet field it belongs to in the chunk byte.
// The chunks come first, then the main packet - always last, always chunk index 0 (xpra
// net/protocol/socket_handler.py: `_add_chunks_to_queue` writes them in that order under the write
// lock, and `process_payload` reassembles them the same way we do here). We ask for this with the
// `chunks` hello capability (see the client's send_hello).
//
// xpra's receive loop rejects a chunk index of 16 or more, and rejects the packet altogether once
// it has stored 4 chunks; mirror both, so a peer that would be refused there is refused here.
const MAX_CHUNKS: usize = 4;
const MAX_CHUNK_INDEX: u8 = 16;


// One logical packet as read off the wire: the main (YAML) payload, plus whatever out-of-band
// chunks preceded it, keyed by the index of the packet field each one belongs to.
pub struct RawPacket {
    pub payload: Vec<u8>,
    pub chunks: HashMap<u8, Vec<u8>>,
}

impl RawPacket {
    // how many bytes this packet took on the wire, payload and chunks together (headers aside)
    pub fn size(&self) -> usize {
        self.payload.len() + self.chunks.values().map(Vec::len).sum::<usize>()
    }
}


pub fn read_packet(stream: &mut Connection) -> Result<RawPacket, Error> {
    let mut chunks: HashMap<u8, Vec<u8>> = HashMap::new();
    let mut received: usize = 0;
    loop {
        let mut header = [0; 8];
        stream.read_exact(&mut header)?;
        trace!("read_packet header={:?}", header);
        // parse header:
        if header[0] != 0x50 {  // "P"
            return Err(Error::new(ErrorKind::InvalidData, format!("invalid packet header byte: {:?}", header[0])));
        }
        let index = header[3];
        let compression = header[2];
        let mut payload_size: usize = 0;
        for i in 0..4 {
            payload_size *= 0x100;
            payload_size += header[i+4] as usize;
        }
        trace!("read_packet index={:?} payload_size={:?}", index, payload_size);
        if index >= MAX_CHUNK_INDEX {
            return Err(Error::new(ErrorKind::InvalidData, format!("invalid chunk index: {:?}", index)));
        }
        // read payload:
        let mut payload = vec![0u8; payload_size];
        let payload_buf: &mut [u8] = payload.as_mut_slice();
        stream.read_exact(payload_buf)?;
        if index == 0 {
            // the main packet, which ends this one: only its header names a packet encoder
            // (chunks are raw binary and are sent with no flags at all).
            if header[1] & FLAGS_YAML == 0 {
                return Err(Error::new(ErrorKind::InvalidData, format!("unsupported packet encoding: {:?}", header[1])));
            }
            if compression != 0 {
                payload = decompress(compression, &payload)?;
            }
            return Ok(RawPacket{ payload, chunks });
        }
        // an out-of-band chunk: hold on to it until the main packet arrives. Count it whether or
        // not we end up keeping it, so a peer can't keep us reading chunks indefinitely.
        received += 1;
        if received >= MAX_CHUNKS {
            return Err(Error::new(ErrorKind::InvalidData, format!("too many chunks: {:?}", received)));
        }
        if chunks.contains_key(&index) {
            return Err(Error::new(ErrorKind::InvalidData, format!("duplicate chunk at index {:?}", index)));
        }
        // Only the network layer's own compression is signalled in the header (xpra's
        // `LevelCompressed`); data that is already compressed by the application - pixels above
        // all - rides here uncompressed as far as we are concerned.
        if compression != 0 {
            match decompress(compression, &payload) {
                Ok(data) => payload = data,
                Err(e) => {
                    // The server compresses with the algorithm we advertised (lz4), bar one case
                    // it never negotiates: clipboard payloads over ~380 bytes, which it brotli
                    // compresses unconditionally (xpra server/source/clipboard.py). Losing the
                    // item beats losing the session, so drop the chunk and carry on - the field
                    // then reads back as empty.
                    warn!("dropping chunk {:?}: {}", index, e);
                    continue;
                }
            }
        }
        chunks.insert(index, payload);
    }
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
    buf.push(0);        // chunk index 0: we never split our own packets into chunks
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
