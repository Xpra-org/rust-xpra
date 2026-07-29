use std::collections::HashMap;
use std::fmt;
use base64::Engine;
use base64::engine::general_purpose;
use log::warn;
use yaml_rust2::Yaml;


// @[derive(Debug)]
#[derive(Clone)]
pub struct Packet {
    pub main: Vec<Yaml>,
    pub raw: HashMap<u8, Vec<u8>>,
    // measured locally by the decode thread for "draw-decoded" packets (microseconds);
    // not part of the xpra wire format, only used to fill in `window-draw-ack`'s decode_time.
    pub decode_time_us: Option<i64>,
}

impl fmt::Debug for Packet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Packet")
            .field("type", &self.main[0])
            .finish()
    }
}


impl Packet {

    pub fn new() -> Self {
        Packet{ main: Vec::new(), raw: HashMap::new(), decode_time_us: None }
    }

    pub fn len(&self) -> usize {
        self.main.len()
    }

    pub fn get_u32(&self, index: u8) -> u32 { yaml_u32(&self.main[index as usize]) }

    pub fn get_i32(&self, index: u8) -> i32 {
        yaml_i32(&self.main[index as usize])
    }

    pub fn get_u64(&self, index: u8) -> u64 {
        yaml_u64(&self.main[index as usize])
    }

    pub fn get_i64(&self, index: u8) -> i64 {
        yaml_i64(&self.main[index as usize])
    }

    pub fn get_str(&self, index: u8) -> String {
        yaml_str(&self.main[index as usize])
    }

    pub fn get_bool(&self, index: u8) -> bool {
        yaml_bool(&self.main[index as usize])
    }

    pub fn get_hash_i32(&self, index: u8, key: String) -> i32 {
        yaml_hash_i32(&self.main[index as usize], key)
    }

    pub fn get_hash_str(&self, index: u8, key: String) -> String {
        yaml_hash_str(&self.main[index as usize], key)
    }

    // Read a boolean from a hash field, tolerating a missing field entirely (e.g. a draw packet's
    // optional trailing options dict). Returns None if the field/key is absent or not a bool.
    pub fn get_hash_bool(&self, index: u8, key: String) -> Option<bool> {
        let i = index as usize;
        if i >= self.main.len() {
            return None;
        }
        yaml_hash_bool(&self.main[i], key)
    }

    pub fn get_bytes(&mut self, index: u8) -> Vec<u8> {
        let raw = self.raw.remove(&index);
        if raw.is_some() {
            return raw.unwrap();
        }
        yaml_bytes(&self.main[index as usize])
    }
}


pub fn yaml_u32(value: &Yaml) -> u32 {
    if let Yaml::Integer(ivalue) = value {
        return *ivalue as u32;
    }
    0
}


pub fn yaml_i32(value: &Yaml) -> i32 {
    if let Yaml::Integer(ivalue) = value {
        return *ivalue as i32;
    }
    0
}


pub fn yaml_u64(value: &Yaml) -> u64 {
    if let Yaml::Integer(ivalue) = value {
        return *ivalue as u64;
    }
    0
}


pub fn yaml_i64(value: &Yaml) -> i64 {
    if let Yaml::Integer(ivalue) = value {
        return *ivalue as i64;
    }
    0
}


pub fn yaml_str(value: &Yaml) -> String {
    if let Yaml::String(s) = value {
        return String::from(s);
    }
    "".to_string()
}


pub fn yaml_bool(value: &Yaml) -> bool {
    match value {
        Yaml::Boolean(b) => *b,
        // some senders use 0/1 rather than a yaml bool (as in yaml_hash_bool):
        Yaml::Integer(i) => *i != 0,
        _ => false,
    }
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
    Vec::new()
}


pub fn yaml_hash_str(value: &Yaml, key: String) -> String {
    if let Yaml::Hash(hash) = value {
        let yaml_key: Yaml = Yaml::String(key);
        let yaml_value = &hash[&yaml_key];
        if let Yaml::String(value) = yaml_value {
            return value.to_string();
        }
    }
    "".to_string()
}

pub fn yaml_hash_bool(value: &Yaml, key: String) -> Option<bool> {
    if let Yaml::Hash(hash) = value {
        let yaml_key: Yaml = Yaml::String(key);
        match hash.get(&yaml_key) {
            Some(Yaml::Boolean(b)) => return Some(*b),
            // some encoders send 0/1 rather than a yaml bool:
            Some(Yaml::Integer(i)) => return Some(*i != 0),
            _ => {}
        }
    }
    None
}

pub fn yaml_hash_i32(value: &Yaml, key: String) -> i32 {
    if let Yaml::Hash(hash) = value {
        let yaml_key: Yaml = Yaml::String(key);
        let yaml_value = &hash[&yaml_key];
        if let Yaml::Integer(ivalue) = yaml_value {
            return *ivalue as i32;
        }
    }
    0
}
