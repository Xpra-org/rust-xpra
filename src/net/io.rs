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


// Generic over the stream rather than taking a `Connection`, so that the framing can be read
// from a plain byte buffer in the tests below. `write_packet` deliberately is *not*: it has to
// keep calling the inherent `Connection::write_all`, which holds the TLS lock across the whole
// packet (see `connection.rs`), and a `W: Write` bound would silently pick `Write::write_all`
// instead and let a concurrent writer interleave its bytes into the middle of a frame.
pub fn read_packet<R: Read>(stream: &mut R) -> Result<RawPacket, Error> {
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


#[cfg(test)]
mod tests {
    use super::{make_header, read_packet, RawPacket, MAX_CHUNKS};
    use std::io::ErrorKind;

    // One framed item as it appears on the wire: the 8-byte header followed by the payload. The
    // fields are taken separately so a test can put something on the wire that `make_header`
    // would never produce - a chunk, a compressed payload, a foreign packet encoder.
    fn frame(flags: u8, compression: u8, index: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0x50, flags, compression, index];
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn read(wire: &[u8]) -> Result<RawPacket, std::io::Error> {
        read_packet(&mut &wire[..])
    }

    // the kind of the error a malformed wire produces. `RawPacket` is not `Debug`, so the Ok
    // side is dropped before unwrapping rather than deriving it just for the tests.
    fn err(wire: &[u8]) -> ErrorKind {
        read(wire).map(|_| ()).unwrap_err().kind()
    }

    #[test]
    fn the_header_is_a_yaml_marker_and_a_big_endian_length() {
        assert_eq!(make_header(b"xy"), vec![0x50, 0x04, 0, 0, 0, 0, 0, 2]);
        // the length is four bytes, most significant first - the one field wide enough to get
        // the byte order wrong without noticing on a small packet
        let header = make_header(&vec![0u8; 0x1234]);
        assert_eq!(&header[4..], &[0x00, 0x00, 0x12, 0x34]);
        // and we never compress or chunk what we send, so those two bytes stay zero
        assert_eq!(header[2], 0);
        assert_eq!(header[3], 0);
    }

    #[test]
    fn a_main_packet_reads_back_its_payload() {
        let mut wire = make_header(b"- hello");
        wire.extend_from_slice(b"- hello");
        let packet = read(&wire).unwrap();
        assert_eq!(packet.payload, b"- hello");
        assert!(packet.chunks.is_empty());
    }

    #[test]
    fn chunks_are_collected_until_the_main_packet_arrives() {
        // the server sends the chunks first and the main packet last, always at index 0
        let mut wire = frame(0, 0, 7, b"pixels");
        wire.extend(frame(0, 0, 2, b"icon"));
        wire.extend(frame(0x04, 0, 0, b"- draw"));
        let packet = read(&wire).unwrap();
        assert_eq!(packet.payload, b"- draw");
        assert_eq!(packet.chunks.get(&7).unwrap(), b"pixels");
        assert_eq!(packet.chunks.get(&2).unwrap(), b"icon");
        assert_eq!(packet.chunks.len(), 2);
    }

    #[test]
    fn size_counts_the_payload_and_every_chunk() {
        let mut wire = frame(0, 0, 7, b"pixels");
        wire.extend(frame(0x04, 0, 0, b"- draw"));
        // 6 bytes of payload plus 6 of chunk, headers not counted
        assert_eq!(read(&wire).unwrap().size(), 12);
    }

    #[test]
    fn a_foreign_magic_byte_is_rejected() {
        // anything that is not an xpra peer - an http server, a wrong port - fails here rather
        // than being parsed as a packet
        let mut wire = frame(0x04, 0, 0, b"- hello");
        wire[0] = b'H';
        assert_eq!(err(&wire), ErrorKind::InvalidData);
    }

    #[test]
    fn a_main_packet_that_is_not_yaml_is_rejected() {
        // we negotiate `encoders: ["yaml"]`, so any other encoder is a protocol violation
        assert_eq!(err(&frame(0x00, 0, 0, b"x")), ErrorKind::InvalidData);
        assert_eq!(err(&frame(0x01, 0, 0, b"x")), ErrorKind::InvalidData);
    }

    #[test]
    fn flags_are_masked_so_flush_rides_alongside_yaml() {
        // the server sets FLAGS_FLUSH (0x8) next to FLAGS_YAML, which is why the check is a mask
        // and not an equality test
        assert_eq!(read(&frame(0x04 | 0x08, 0, 0, b"- hi")).unwrap().payload, b"- hi");
    }

    #[test]
    fn a_chunk_index_of_sixteen_or_more_is_rejected() {
        // mirrors the limit in xpra's own receive loop
        assert_eq!(err(&frame(0, 0, 16, b"x")), ErrorKind::InvalidData);
    }

    #[test]
    fn more_chunks_than_the_limit_are_rejected() {
        // the count is checked before the chunk is stored, so the limit is MAX_CHUNKS - 1 kept
        let mut wire = Vec::new();
        for index in 1..MAX_CHUNKS as u8 {
            wire.extend(frame(0, 0, index, b"x"));
        }
        wire.extend(frame(0x04, 0, 0, b"- ok"));
        assert_eq!(read(&wire).unwrap().chunks.len(), MAX_CHUNKS - 1);

        // one more and the packet is refused outright
        let mut wire = Vec::new();
        for index in 1..=MAX_CHUNKS as u8 {
            wire.extend(frame(0, 0, index, b"x"));
        }
        wire.extend(frame(0x04, 0, 0, b"- ok"));
        assert_eq!(err(&wire), ErrorKind::InvalidData);
    }

    #[test]
    fn a_repeated_chunk_index_is_rejected() {
        let mut wire = frame(0, 0, 3, b"first");
        wire.extend(frame(0, 0, 3, b"second"));
        wire.extend(frame(0x04, 0, 0, b"- draw"));
        assert_eq!(err(&wire), ErrorKind::InvalidData);
    }

    #[test]
    fn an_lz4_payload_is_decompressed() {
        // xpra's framing is a 4-byte little-endian uncompressed size followed by a raw block,
        // and the header's high bits name the algorithm (0x10 = lz4) with the level in the low
        // nibble - so a level of 1 reads as 0x11.
        let body = b"- hello\n- hello\n- hello\n";
        let compressed = lz4_flex::block::compress_prepend_size(body);
        assert_eq!(read(&frame(0x04, 0x11, 0, &compressed)).unwrap().payload, body);
        // and a chunk carries its own compression the same way
        let mut wire = frame(0, 0x10, 5, &compressed);
        wire.extend(frame(0x04, 0, 0, b"- draw"));
        assert_eq!(read(&wire).unwrap().chunks.get(&5).unwrap(), body);
    }

    #[test]
    fn an_unsupported_compressor_on_the_main_packet_is_fatal() {
        // we advertise only lz4, so brotli (0x40) or zstd (0x80) on the main packet means the
        // server ignored the negotiation and nothing further can be trusted
        for algorithm in [0x40, 0x80] {
            assert_eq!(err(&frame(0x04, algorithm, 0, b"junk")),
                       ErrorKind::InvalidData);
        }
    }

    #[test]
    fn an_unsupported_compressor_on_a_chunk_drops_only_that_chunk() {
        // the server brotli-compresses large clipboard payloads without negotiating it, and
        // losing a paste has to beat losing the session
        let mut wire = frame(0, 0x40, 4, b"brotli");
        wire.extend(frame(0, 0, 6, b"kept"));
        wire.extend(frame(0x04, 0, 0, b"- clipboard"));
        let packet = read(&wire).unwrap();
        assert_eq!(packet.payload, b"- clipboard");
        assert!(packet.chunks.get(&4).is_none());
        assert_eq!(packet.chunks.get(&6).unwrap(), b"kept");
    }

    #[test]
    fn a_truncated_payload_is_an_error() {
        // a server killed mid-packet must not read back as a short one
        let mut wire = make_header(b"- hello");
        wire.extend_from_slice(b"- hel");
        assert_eq!(err(&wire), ErrorKind::UnexpectedEof);
        // the same for a header cut in half
        assert_eq!(err(&[0x50, 0x04, 0]), ErrorKind::UnexpectedEof);
    }
}
