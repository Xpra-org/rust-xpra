use std::io::{Error, ErrorKind};
use std::result::{Result};
use std::{str};
use log::{error};
use yaml_rust2::{YamlLoader, Yaml};
use crate::net::io::RawPacket;
use crate::net::packet::Packet;


pub const VERSION_KEY_STR: &str = "version";


// Turn one packet read off the wire into a `Packet`: its YAML payload gives the positional
// fields, and any out-of-band chunks that came with it are kept aside in `raw`, keyed by the
// field index they belong to. `Packet::get_bytes` reads those in preference to the (empty)
// placeholder the sender left in the YAML - see net::io.
pub fn parse_packet(raw: RawPacket) -> Result<Packet, Error> {
    let RawPacket{ mut payload, chunks } = raw;
    let payload_buf: &mut [u8] = payload.as_mut_slice();
    let payload_str = match str::from_utf8(payload_buf) {
        Ok(payload_str) => payload_str,
        Err(e) => return Err(Error::new(ErrorKind::InvalidData, e)),
    };
    let ret = YamlLoader::load_from_str(payload_str);
    if ! ret.is_ok() {
        return Err(Error::new(ErrorKind::InvalidData, ret.unwrap_err()));
    }
    let yaml_packet = ret.unwrap();
    if yaml_packet.len() != 1 {
        error!("expected 1 item, got {:?}", yaml_packet.len());
        return Err(Error::new(ErrorKind::InvalidData, "too many items"));
    }
    let packet = &yaml_packet[0];
    // error!("packet = {:?}", packet);
    match packet {
        Yaml::Array(array) => {
            Ok(Packet{ main: array.to_vec(), raw: chunks, decode_time_us: None })
        },
        _ => {
            error!("packet is not an array: {:?}", packet);
            Err(Error::new(ErrorKind::InvalidData, "received invalid packet data type"))
        },
    }
}


#[cfg(test)]
mod tests {
    use super::parse_packet;
    use crate::net::io::RawPacket;
    use std::collections::HashMap;

    fn raw(payload: &[u8], chunks: HashMap<u8, Vec<u8>>) -> RawPacket {
        RawPacket { payload: payload.to_vec(), chunks }
    }

    fn parse(payload: &[u8]) -> Result<crate::net::packet::Packet, std::io::Error> {
        parse_packet(raw(payload, HashMap::new()))
    }

    #[test]
    fn a_packet_is_a_yaml_array_of_positional_fields() {
        let packet = parse(b"- draw\n- 7\n- png\n").unwrap();
        assert_eq!(packet.len(), 3);
        assert_eq!(packet.get_str(0), "draw");
        assert_eq!(packet.get_u32(1), 7);
    }

    #[test]
    fn the_chunks_are_carried_over_into_raw() {
        let mut chunks = HashMap::new();
        chunks.insert(7, b"pixels".to_vec());
        let packet = parse_packet(raw(b"- draw\n- 7\n", chunks)).unwrap();
        assert_eq!(packet.raw.get(&7).unwrap(), b"pixels");
    }

    #[test]
    fn a_payload_that_is_not_an_array_is_rejected() {
        // every xpra packet is a list whose first item names the type
        assert!(parse(b"draw: 7\n").is_err());
        assert!(parse(b"hello\n").is_err());
    }

    #[test]
    fn malformed_input_is_rejected_rather_than_parsed() {
        // invalid utf-8
        assert!(parse(&[0xff, 0xfe]).is_err());
        // invalid yaml
        assert!(parse(b"- [unclosed\n").is_err());
        // more than one document in one payload
        assert!(parse(b"- a\n---\n- b\n").is_err());
    }
}
