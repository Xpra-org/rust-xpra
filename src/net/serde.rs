use std::io::{Error, ErrorKind};
use std::result::{Result};
use std::{str};
use std::collections::HashMap;
use log::{error};
use yaml_rust2::{YamlLoader, Yaml};
use crate::net::packet::Packet;


pub const VERSION_KEY_STR: &str = "version";


pub fn parse_payload(mut payload: Vec<u8>) -> Result<Packet, Error> {
    let payload_buf: &mut [u8] = payload.as_mut_slice();
    let payload_str = str::from_utf8(payload_buf);
    let ret = YamlLoader::load_from_str(payload_str.unwrap());
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
            return Ok(Packet{ main: array.to_vec(), raw: HashMap::new() });
        },
        _ => {
            error!("packet is not an array: {:?}", packet);
            return Err(Error::new(ErrorKind::InvalidData, "received invalid packet data type"));
        },
    }
}
