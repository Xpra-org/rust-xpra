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
