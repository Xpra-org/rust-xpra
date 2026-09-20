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


// Look up a key in a hash, without assuming anything about the value's type: for the nested caps
// dicts (`mmap.write.token`) and packet options (a draw packet's `chunks` list) that the typed
// accessors below cannot reach.
pub fn yaml_hash<'a>(value: &'a Yaml, key: &str) -> Option<&'a Yaml> {
    if let Yaml::Hash(hash) = value {
        return hash.get(&Yaml::String(key.to_string()));
    }
    None
}

// The string entries of a list-valued key, skipping anything that is not a string: xpra sends
// its capability lists (a server's picture encodings, its packet types) as tuples of strings.
pub fn yaml_hash_strings(value: &Yaml, key: &str) -> Vec<String> {
    match yaml_hash(value, key) {
        Some(Yaml::Array(values)) => values.iter()
            .filter_map(|item| match item {
                Yaml::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub fn yaml_hash_str(value: &Yaml, key: String) -> String {
    match yaml_hash(value, &key) {
        Some(Yaml::String(s)) => s.to_string(),
        _ => "".to_string(),
    }
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
    match yaml_hash(value, &key) {
        Some(Yaml::Integer(i)) => *i as i32,
        _ => 0,
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use yaml_rust2::YamlLoader;

    fn yaml(source: &str) -> Yaml {
        YamlLoader::load_from_str(source).unwrap().remove(0)
    }

    #[test]
    fn a_missing_hash_key_reads_as_absent() {
        // the accessors index the hash directly, so a key the server did not send must not take
        // the process down - every one of these is reached with optional capability dicts
        let caps = yaml("{present: 1}");
        assert_eq!(yaml_hash_str(&caps, "absent".to_string()), "");
        assert_eq!(yaml_hash_i32(&caps, "absent".to_string()), 0);
        assert_eq!(yaml_hash_bool(&caps, "absent".to_string()), None);
        assert_eq!(yaml_hash(&caps, "absent"), None);
        assert!(yaml_hash_strings(&caps, "absent").is_empty());
    }

    #[test]
    fn a_value_of_the_wrong_type_reads_as_the_default() {
        // the wire is untyped, so every accessor falls back rather than failing
        let text = yaml("hello");
        assert_eq!(yaml_u32(&text), 0);
        assert_eq!(yaml_i32(&text), 0);
        assert_eq!(yaml_u64(&text), 0);
        assert_eq!(yaml_i64(&text), 0);
        assert_eq!(yaml_str(&yaml("42")), "");
        assert!(yaml_bytes(&yaml("42")).is_empty());
    }

    #[test]
    fn booleans_are_accepted_as_numbers_too() {
        // some senders put 0/1 on the wire where a yaml bool belongs
        assert!(yaml_bool(&yaml("true")));
        assert!(!yaml_bool(&yaml("false")));
        assert!(yaml_bool(&yaml("1")));
        assert!(!yaml_bool(&yaml("0")));
        assert!(!yaml_bool(&yaml("hello")));
        assert_eq!(yaml_hash_bool(&yaml("{a: 1, b: 0}"), "a".to_string()), Some(true));
        assert_eq!(yaml_hash_bool(&yaml("{a: 1, b: 0}"), "b".to_string()), Some(false));
    }

    #[test]
    fn binary_values_are_base64_with_the_line_breaks_removed() {
        // yaml wraps long !!binary scalars, so the newlines have to go before decoding
        assert_eq!(yaml_bytes(&yaml("\"aGVsbG8=\"")), b"hello");
        assert_eq!(yaml_bytes(&yaml("\"aGVs\\nbG8=\"")), b"hello");
        // junk is dropped rather than propagated
        assert!(yaml_bytes(&yaml("\"not base64!\"")).is_empty());
    }

    #[test]
    fn a_list_field_keeps_only_its_strings() {
        assert_eq!(yaml_hash_strings(&yaml("{encodings: [png, 7, jpeg]}"), "encodings"),
                   vec!["png".to_string(), "jpeg".to_string()]);
        // a key whose value is not a list at all
        assert!(yaml_hash_strings(&yaml("{encodings: png}"), "encodings").is_empty());
    }

    #[test]
    fn an_out_of_band_chunk_wins_over_the_yaml_placeholder() {
        // the sender leaves an empty placeholder in the payload and sends the real bytes as a
        // chunk, so `get_bytes` has to prefer the chunk - see net::io
        let mut packet = Packet::new();
        packet.main = vec![yaml("draw"), yaml("\"aGVsbG8=\"")];
        packet.raw.insert(1, b"real pixels".to_vec());
        assert_eq!(packet.get_bytes(1), b"real pixels");
        // and falls back to the payload once the chunk has been taken
        assert_eq!(packet.get_bytes(1), b"hello");
    }

    #[test]
    fn an_absent_trailing_field_reads_as_absent() {
        // a draw packet's options dict is optional, so indexing past the end must not panic
        let mut packet = Packet::new();
        packet.main = vec![yaml("draw")];
        assert_eq!(packet.len(), 1);
        assert_eq!(packet.get_hash_bool(9, "flush".to_string()), None);
    }
}
