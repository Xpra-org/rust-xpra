// #![windows_subsystem = "windows"]

use std::io::{Error, ErrorKind};
use log::{error, warn};
use std::result::{Result};
use std::{str};
use yaml_rust2::{YamlLoader, Yaml};
use base64::engine::{Engine, general_purpose};
// use bytes::Bytes;


pub const VERSION_KEY_STR: &str = "version";


pub fn parse_payload(mut payload: Vec<u8>) -> Result<Vec<Yaml>, Error> {
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
            return Ok(array.to_vec());
        },
        _ => {
            error!("packet is not an array: {:?}", packet);
            return Err(Error::new(ErrorKind::InvalidData, "received invalid packet data type"));
        },
    }
}


pub fn yaml_i32(value: &Yaml) -> i32 {
    if let Yaml::Integer(ivalue) = value {
        return *ivalue as i32;
    }
    return 0;
}


pub fn yaml_i64(value: &Yaml) -> i64 {
    if let Yaml::Integer(ivalue) = value {
        return *ivalue as i64;
    }
    return 0;
}


pub fn yaml_str(value: &Yaml) -> String {
    if let Yaml::String(s) = value {
        return String::from(s);
    }
    return "".to_string();
}


pub fn yaml_bytes(value: &Yaml) -> Vec<u8> {
    if let Yaml::String(s) = value {
        let sval = String::from(s);
        let nonl = sval.replace("\n", "");
        match general_purpose::STANDARD.decode(nonl) {
            Ok(bytes) => return bytes,
            Err(e) => {
                warn!("failed to decode yaml bytes: {:?}", e);
                return Vec::new();
            }
        };
    }
    return Vec::new();
}


pub fn yaml_hash_str(value: &Yaml, key: String) -> String {
    if let Yaml::Hash(hash) = value {
        let yaml_key: Yaml = Yaml::String(key);
        let yaml_value = &hash[&yaml_key];
        if let Yaml::String(value) = yaml_value {
            return value.to_string();
        }
    }
    return "".to_string();
}
