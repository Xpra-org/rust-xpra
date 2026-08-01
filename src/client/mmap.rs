// Shared-memory ("mmap") picture transfers, in the server -> client direction.
//
// When the server runs on the same host as this client, sending pixels down the socket - encoded
// with jpeg/webp/h264 and decoded again here - is pure waste. Instead we create a file, map it
// shared, and hand its path to the server in our `hello`; the server then writes raw BGRX frames
// straight into it and its `draw` packets carry only (offset, length) pairs into that area.
// See xpra's own `net/mmap/{common,io,objects}.py`, `client/subsystem/mmap.py` and
// `server/source/mmap.py`, and `docs/Subsystems/MMAP.md` upstream.
//
// Only the *read* area (server -> client) is implemented: the server's own read area is used
// exclusively for webcam frames and the encoder subsystem, neither of which this client has.
//
// Layout of the area, which is a ring buffer:
//
//     offset 0: data_start, u32, native byte order - written by *us*, how far we have consumed
//     offset 4: data_end,   u32, native byte order - written by the server, how far it has written
//     offset 8..size:       the pixel data
//
// The server never reclaims space until we move `data_start` past it, so *every* mmap draw has to
// end in a `release()` - including the ones we fail to paint (see xpra's comment in
// `server/window/compress.py`: "never cancel mmap after encoding because we need to reclaim the
// space by getting the client to move the mmap received pointer").
//
// POSIX only. mmap only helps when both ends share a host, which on Windows would mean an xpra
// shadow server on the same machine, talking over a *named file mapping* rather than a file;
// `create` simply returns None there and we advertise no mmap capability at all.

// ... which also means that on Windows nothing below ever runs:
#![cfg_attr(not(unix), allow(dead_code))]

use std::env;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use log::{debug, warn};
use serde_json::{json, Value};
use yaml_rust2::Yaml;

use xpra::net::packet::{yaml_hash, yaml_hash_bool};
use xpra::net::rand::secure_random_bytes;

// the two u32 control words at the start of the area; pixel data starts after them.
const HEADER_SIZE: usize = 8;
// the length of the token we write, in bytes. xpra's default is 128 (it writes a 128 bit uuid,
// zero-padded); its reader is length-parametric, so 8 bytes is just as valid and keeps our token
// inside the integer range our JSON-as-YAML writer can emit.
const TOKEN_BYTES: usize = 8;
// the server discards anything smaller (`MMAP_Server.min_size`, xpra server/subsystem/mmap.py).
const MIN_SIZE: usize = 64 * 1024 * 1024;
const DEFAULT_SIZE: usize = 128 * 1024 * 1024;
// the ring offsets in the header are u32, so nothing beyond 4GB can be addressed.
const MAX_SIZE: usize = u32::MAX as usize;

pub struct MmapArea {
    // the MAP_SHARED mapping.
    ptr: *mut u8,
    size: usize,
    // the backing file, sent as the `file` capability and unlinked once the server has it open.
    path: String,
    // false when the user pointed XPRA_MMAP at a pre-existing file (a virtio-shmem device, say):
    // we did not create it, so we must not remove it.
    delete: bool,
    // whether `unlink` has already run. It is called through a shared reference (the UI thread
    // holds one `Arc`, the decode thread the other) and again from `Drop`, so it has to be both
    // idempotent and callable without `&mut self`.
    unlinked: AtomicBool,
    // the file only has to outlive the mapping's creation, but holding on to it costs nothing.
    _file: Option<std::fs::File>,
    // our own token, sent in the hello for the server to verify.
    token: u64,
    token_index: usize,
}

// The raw pointer is what makes these necessary. The UI thread touches the area only during the
// handshake (writing our token, verifying the server's); the decode thread touches it only
// afterwards, once draws start arriving - the two never overlap. The control word is written
// through an atomic.
unsafe impl Send for MmapArea {}
unsafe impl Sync for MmapArea {}

#[cfg(unix)]
unsafe extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, length: usize, prot: std::ffi::c_int,
            flags: std::ffi::c_int, fd: std::ffi::c_int, offset: isize) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> std::ffi::c_int;
}

impl MmapArea {

    // Create (or open) the backing file and map it. Returns None when mmap is turned off, when the
    // platform has no support, or when anything at all goes wrong - a failure here is never fatal,
    // it just means the session falls back to the usual encodings.
    pub fn create() -> Option<MmapArea> {
        #[cfg(not(unix))]
        {
            debug!("mmap picture transfers are not supported on this platform");
            None
        }
        #[cfg(unix)]
        {
            match Self::do_create() {
                Ok(area) => Some(area),
                Err(message) => {
                    // an empty message means "switched off", which needs no warning:
                    if !message.is_empty() {
                        warn!("not using mmap picture transfers: {}", message);
                    }
                    None
                }
            }
        }
    }

    #[cfg(unix)]
    fn do_create() -> Result<MmapArea, String> {
        use std::fs::OpenOptions;
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        use std::path::Path;
        use log::info;
        use xpra::net::rand::secure_hex;

        // remove a file we made ourselves, on the way out of a failed setup:
        fn failed(path: &str, delete: bool, message: String) -> Result<MmapArea, String> {
            if delete {
                let _ = std::fs::remove_file(path);
            }
            Err(message)
        }

        // XPRA_MMAP: a false value turns mmap off, an absolute path names the backing file (which
        // may already exist - that is how xpra's virtio-shmem setup is used), anything else just
        // means "on", with a file of our own making. Mirrors the python client's --mmap option.
        let option = env::var("XPRA_MMAP").unwrap_or_default();
        if matches!(option.to_lowercase().trim(), "no" | "false" | "0" | "off") {
            return Err(String::new());
        }
        let (path, existing) = if option.starts_with('/') {
            let exists = Path::new(&option).exists();
            (option.clone(), exists)
        } else {
            // xpra puts its own areas in the platform temporary directory, honouring XPRA_MMAP_DIR.
            let dir = env::var("XPRA_MMAP_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| env::temp_dir());
            let joined = dir.join(format!("xpra-{}.mmap", secure_hex(16)));
            match joined.to_str() {
                Some(path) => (path.to_string(), false),
                // the path travels as a JSON string, so a non-UTF-8 one is unusable:
                None => return Err(format!("temporary directory {:?} is not valid utf-8", dir)),
            }
        };
        let (file, size) = if existing {
            // an area someone else made (a shared memory device): take it as it is.
            let file = OpenOptions::new().read(true).write(true).open(&path)
                .map_err(|e| format!("cannot open {:?}: {}", path, e))?;
            let size = file.metadata().map_err(|e| format!("cannot stat {:?}: {}", path, e))?
                .len() as usize;
            info!("using the existing mmap file {:?}: {}", path, size_str(size));
            (file, size)
        } else {
            let mut size = env_size("XPRA_MMAP_SIZE").unwrap_or(DEFAULT_SIZE);
            if size < MIN_SIZE || size > MAX_SIZE {
                let clamped = size.clamp(MIN_SIZE, MAX_SIZE);
                warn!("mmap size {} is out of range, using {}", size_str(size), size_str(clamped));
                size = clamped;
            }
            let file = OpenOptions::new()
                .read(true).write(true).create_new(true).mode(0o600)
                .open(&path)
                .map_err(|e| format!("cannot create {:?}: {}", path, e))?;
            // a sparse file: its pages only cost anything once they are actually written to.
            if let Err(e) = file.set_len(size as u64) {
                return failed(&path, true, format!("cannot size {:?} to {} bytes: {}",
                                                   path, size, e));
            }
            (file, size)
        };
        if size < MIN_SIZE || size > MAX_SIZE {
            return failed(&path, !existing,
                          format!("unusable mmap size {}: it must be between {} and {}",
                                  size_str(size), size_str(MIN_SIZE), size_str(MAX_SIZE)));
        }
        // PROT_READ|PROT_WRITE and MAP_SHARED have these values on every POSIX platform we build
        // for, and this is the only libc call the module makes - not worth a dependency, in a
        // crate that hand-rolls sha1/sha256/websockets for the same reason.
        const PROT_READ_WRITE: std::ffi::c_int = 1 | 2;
        const MAP_SHARED: std::ffi::c_int = 1;
        let ptr = unsafe {
            mmap(std::ptr::null_mut(), size, PROT_READ_WRITE, MAP_SHARED, file.as_raw_fd(), 0)
        };
        // MAP_FAILED is (void *) -1:
        if ptr.is_null() || ptr as isize == -1 {
            return failed(&path, !existing,
                          format!("failed to map {:?}: {}", path, std::io::Error::last_os_error()));
        }
        let mut area = MmapArea {
            ptr: ptr as *mut u8,
            size,
            path,
            delete: !existing,
            unlinked: AtomicBool::new(false),
            _file: Some(file),
            token: 0,
            token_index: 0,
        };
        area.gen_token();
        debug!("mmap area {:?} of {} ready, token at {}", area.path, size_str(size),
               area.token_index);
        Ok(area)
    }

    // Pick our token and write it into the area for the server to find. The index stays clear of
    // the control header - xpra's own gen_token() picks from the whole area and can land on it.
    fn gen_token(&mut self) {
        self.token = random_u64();
        let range = (self.size - HEADER_SIZE - TOKEN_BYTES + 1) as u64;
        self.token_index = HEADER_SIZE + (random_u64() % range) as usize;
        let (token, index) = (self.token, self.token_index);
        write_token(self.bytes_mut(), token, index, TOKEN_BYTES);
    }

    // What goes into our hello as `mmap.read` - the area the server writes to and we read from.
    pub fn caps(&self) -> Value {
        json!({
            "file": self.path,
            "size": self.size,
            "token": self.token,
            "token_index": self.token_index,
            "token_bytes": TOKEN_BYTES,
        })
    }

    pub fn size(&self) -> usize {
        self.size
    }

    // Check the token the server wrote back, given the whole server hello capabilities hash.
    // Ok(true): the server is using the area and its token checks out.
    // Ok(false): the server is not using it (no capability, or explicitly disabled).
    // Err(..): the token is wrong - the path resolved to a *different* file, so the server is
    // writing pixels somewhere we cannot see. Fatal: it still believes mmap is live.
    pub fn check_server_caps(&self, hello: &Yaml) -> Result<bool, String> {
        let caps = match yaml_hash(hello, "mmap") {
            Some(caps) => caps,
            None => return Ok(false),
        };
        // the server describes the area from its own point of view, so *its* write area is *our*
        // read area. The unprefixed form is what xpra < 6.3 sends (and what a backwards-compatible
        // server duplicates alongside the prefixed one).
        let area = yaml_hash(caps, "write").unwrap_or(caps);
        if !yaml_hash_bool(area, "enabled".to_string()).unwrap_or(true) {
            return Ok(false);
        }
        let token = match yaml_hash(area, "token").and_then(yaml_u128) {
            Some(token) => token,
            // no token at all: the server did not take up the offer.
            None => return Ok(false),
        };
        let index = hash_usize(area, "token_index").unwrap_or(0);
        // xpra always writes 128 bytes (a zero-padded 128 bit uuid), which is also its default:
        let count = hash_usize(area, "token_bytes").unwrap_or(128);
        match read_token(self.bytes(), index, count) {
            Some(found) if found == token => Ok(true),
            // a zero token means the server left the area alone (xpra's own verify_token):
            Some(0) => Ok(false),
            Some(found) => Err(format!("token verification failed: expected {:x}, found {:x}",
                                       token, found)),
            None => Err(format!("invalid token position: {} bytes at {}", count, index)),
        }
    }

    // Copy one image out of the area, de-striding it into a tightly packed w*h*4 buffer - the same
    // shape turbojpeg, spng and libwebp hand back, so the rest of the draw path is unchanged.
    pub fn read_image(&self, chunks: &[(usize, usize)], w: usize, h: usize, rowstride: usize)
            -> Result<Vec<u8>, String> {
        destride(self.bytes(), chunks, w, h, rowstride)
    }

    // Move `data_start` past the chunks we have just consumed, which is what lets the server reuse
    // that part of the ring. Must be called for every mmap draw, painted or not.
    pub fn release(&self, chunks: &[(usize, usize)]) {
        let end = match chunks.last() {
            Some((offset, length)) => (offset + length) as u32,
            None => return,
        };
        // Release ordering: the server must not see the new data_start - and start overwriting -
        // before our reads above have retired. Native byte order, matching the c_uint32 view xpra
        // takes of the same four bytes (we are by definition on the same machine).
        unsafe { AtomicU32::from_ptr(self.ptr as *mut u32) }.store(end, Ordering::Release);
    }

    // Remove the backing file while keeping the mapping. Called once the handshake is over: the
    // server has the file open by then, and neither side needs the directory entry any more.
    pub fn unlink(&self) {
        if !self.delete || self.unlinked.swap(true, Ordering::Relaxed) {
            return;
        }
        if let Err(e) = std::fs::remove_file(&self.path) {
            warn!("failed to remove the mmap file {:?}: {}", self.path, e);
        }
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.size) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.size) }
    }
}

impl Drop for MmapArea {
    fn drop(&mut self) {
        self.unlink();
        #[cfg(unix)]
        if !self.ptr.is_null() {
            unsafe { munmap(self.ptr as *mut std::ffi::c_void, self.size) };
        }
    }
}


// The (offset, length) pairs an mmap `draw` packet carries in place of pixel data. `value` is the
// packet's `options["chunks"]` - or, for the copy the server also leaves in the packet's data
// field for older clients, that field.
pub fn parse_chunks(value: &Yaml) -> Result<Vec<(usize, usize)>, String> {
    let array = match value {
        Yaml::Array(array) => array,
        _ => return Err("mmap draw packet without a chunk list".to_string()),
    };
    let mut chunks = Vec::with_capacity(array.len());
    for chunk in array {
        match chunk {
            Yaml::Array(pair) if pair.len() == 2 => match (&pair[0], &pair[1]) {
                (Yaml::Integer(offset), Yaml::Integer(length)) if *offset >= 0 && *length >= 0 =>
                    chunks.push((*offset as usize, *length as usize)),
                _ => return Err(format!("invalid mmap chunk {:?}", chunk)),
            },
            _ => return Err(format!("invalid mmap chunk {:?}", chunk)),
        }
    }
    if chunks.is_empty() {
        return Err("empty mmap chunk list".to_string());
    }
    Ok(chunks)
}


// Copy a tightly packed w*h*4 image out of the logical byte stream formed by concatenating
// `chunks`. `rowstride` is the *source* stride, which for a damage sub-rectangle is the stride of
// the whole window rather than w*4: xpra hands out zero-copy sub-images that keep their parent's
// stride (XImageWrapper.get_sub_image). Every other encoding this client decodes produces tightly
// packed output, which is why the draw packet's rowstride field is only ever read here.
fn destride(area: &[u8], chunks: &[(usize, usize)], w: usize, h: usize, rowstride: usize)
        -> Result<Vec<u8>, String> {
    if w == 0 || h == 0 {
        return Err(format!("empty mmap draw packet: {}x{}", w, h));
    }
    let width_bytes = w.checked_mul(4).ok_or("mmap draw packet width overflows")?;
    let stride = if rowstride == 0 { width_bytes } else { rowstride };
    if stride < width_bytes {
        return Err(format!("mmap rowstride {} is smaller than {}x4 bytes", stride, w));
    }
    let mut available = 0usize;
    for (offset, length) in chunks {
        // the pixel data lives after the control header, and inside the area:
        if *offset < HEADER_SIZE || offset.checked_add(*length).unwrap_or(usize::MAX) > area.len() {
            return Err(format!("mmap chunk ({}, {}) is out of range", offset, length));
        }
        available += length;
    }
    // the last row only needs `width_bytes`, not a whole stride:
    let required = (h - 1) * stride + width_bytes;
    if available < required {
        return Err(format!("mmap chunks are too small: {} bytes for a {}x{} image with a stride \
                            of {} ({} needed)", available, w, h, stride, required));
    }
    let mut out = vec![0u8; width_bytes * h];
    for row in 0..h {
        let to = &mut out[row * width_bytes..(row + 1) * width_bytes];
        copy_logical(area, chunks, row * stride, to);
    }
    Ok(out)
}

// Copy `dst.len()` bytes starting at logical offset `start` in the concatenation of `chunks`.
// The server wraps the ring around, so it will split one image into two chunks at an arbitrary
// byte boundary - and a single row can straddle the two, which is why this works on logical
// offsets rather than on each chunk in turn. The caller has bounds-checked every chunk and the
// total length already.
fn copy_logical(area: &[u8], chunks: &[(usize, usize)], start: usize, dst: &mut [u8]) {
    let mut skip = start;
    let mut written = 0usize;
    for (offset, length) in chunks {
        if skip >= *length {
            skip -= length;
            continue;
        }
        let from = offset + skip;
        let take = (length - skip).min(dst.len() - written);
        dst[written..written + take].copy_from_slice(&area[from..from + take]);
        written += take;
        skip = 0;
        if written == dst.len() {
            return;
        }
    }
}

// xpra writes tokens one byte at a time, least significant first, zero-padded out to `count`
// bytes (write_mmap_token / read_mmap_token, xpra net/mmap/io.py).
fn write_token(area: &mut [u8], token: u64, index: usize, count: usize) {
    let mut value = token;
    for i in 0..count {
        area[index + i] = (value & 0xff) as u8;
        value >>= 8;
    }
}

// Read back a token of `count` bytes. None if it does not fit in the area, or if it is wider than
// 128 bits - which xpra's never is, since it writes a uuid4 zero-padded out to 128 bytes.
fn read_token(area: &[u8], index: usize, count: usize) -> Option<u128> {
    if count == 0 || index.checked_add(count)? > area.len() {
        return None;
    }
    let mut token: u128 = 0;
    for i in 0..count {
        let byte = area[index + i];
        if i >= 16 {
            if byte != 0 {
                return None;
            }
            continue;
        }
        token |= (byte as u128) << (8 * i);
    }
    Some(token)
}

// The server's token is a 128 bit uuid, so it does not fit in an i64: yaml-rust2 parses a decimal
// that big as a Yaml::Real holding the original digits rather than as a Yaml::Integer.
fn yaml_u128(value: &Yaml) -> Option<u128> {
    match value {
        Yaml::Integer(i) if *i >= 0 => Some(*i as u128),
        Yaml::Real(s) | Yaml::String(s) => s.parse::<u128>().ok(),
        _ => None,
    }
}

fn hash_usize(value: &Yaml, key: &str) -> Option<usize> {
    match yaml_hash(value, key)? {
        Yaml::Integer(i) if *i >= 0 => Some(*i as usize),
        _ => None,
    }
}

// A size in bytes from the environment, accepting a plain byte count or a K/M/G suffix.
fn env_size(name: &str) -> Option<usize> {
    let value = env::var(name).ok()?;
    let trimmed = value.trim();
    let (digits, scale) = match trimmed.chars().last()? {
        'k' | 'K' => (&trimmed[..trimmed.len() - 1], 1024),
        'm' | 'M' => (&trimmed[..trimmed.len() - 1], 1024 * 1024),
        'g' | 'G' => (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024),
        _ => (trimmed, 1),
    };
    match digits.trim().parse::<usize>() {
        Ok(size) => Some(size.saturating_mul(scale)),
        Err(_) => {
            warn!("ignoring invalid {}={:?}", name, value);
            None
        }
    }
}

fn size_str(size: usize) -> String {
    format!("{}MB", size / 1024 / 1024)
}

fn random_u64() -> u64 {
    let mut buf = [0u8; 8];
    secure_random_bytes(&mut buf);
    u64::from_le_bytes(buf)
}


#[cfg(test)]
mod tests {
    use super::*;

    // an area whose bytes vary with their offset, so a mis-read shows up as the wrong value:
    fn area(size: usize) -> Vec<u8> {
        (0..size).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn destride_tightly_packed() {
        let src = area(4096);
        let out = destride(&src, &[(8, 2 * 3 * 4)], 3, 2, 12).unwrap();
        assert_eq!(out, src[8..8 + 24].to_vec());
    }

    #[test]
    fn destride_zero_rowstride_means_packed() {
        let src = area(4096);
        let out = destride(&src, &[(8, 24)], 3, 2, 0).unwrap();
        assert_eq!(out, src[8..32].to_vec());
    }

    // a damage sub-rectangle keeps the whole window's stride, so every row but the last is
    // followed by pixels belonging to either side of the rectangle:
    #[test]
    fn destride_wider_stride() {
        let src = area(4096);
        let (w, h, stride) = (2usize, 3usize, 40usize);
        let out = destride(&src, &[(8, (h - 1) * stride + w * 4)], w, h, stride).unwrap();
        assert_eq!(out.len(), w * h * 4);
        for row in 0..h {
            let from = 8 + row * stride;
            assert_eq!(&out[row * w * 4..(row + 1) * w * 4], &src[from..from + w * 4]);
        }
    }

    // the server wraps the ring around and splits the image in two at an arbitrary byte offset -
    // one that can fall in the middle of a row:
    #[test]
    fn destride_wrapped_chunks() {
        let src = area(4096);
        let (w, h) = (4usize, 4usize);
        let total = w * h * 4;
        let split = 22;
        let chunks = [(1000, split), (8, total - split)];
        let out = destride(&src, &chunks, w, h, 0).unwrap();
        let mut expected = src[1000..1000 + split].to_vec();
        expected.extend_from_slice(&src[8..8 + total - split]);
        assert_eq!(out, expected);
    }

    #[test]
    fn destride_rejects_bad_chunks() {
        let src = area(4096);
        // not enough bytes for the image:
        assert!(destride(&src, &[(8, 16)], 4, 4, 0).is_err());
        // reaching past the end of the area:
        assert!(destride(&src, &[(4000, 256)], 4, 4, 0).is_err());
        // overlapping the control header:
        assert!(destride(&src, &[(4, 256)], 4, 4, 0).is_err());
        // a stride narrower than the image:
        assert!(destride(&src, &[(8, 256)], 4, 4, 8).is_err());
    }

    #[test]
    fn token_round_trip() {
        let mut src = vec![0u8; 4096];
        write_token(&mut src, 0x0123_4567_89ab_cdef, 100, TOKEN_BYTES);
        assert_eq!(src[100], 0xef);
        assert_eq!(src[107], 0x01);
        assert_eq!(read_token(&src, 100, TOKEN_BYTES), Some(0x0123_4567_89ab_cdef));
        // a count that runs past the end of the area:
        assert_eq!(read_token(&src, 100, 4096), None);
    }

    // what the server actually sends: a 128 bit uuid, zero-padded out to 128 bytes.
    #[test]
    fn server_token_is_128_bits() {
        let mut src = vec![0u8; 4096];
        let token: u128 = 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210;
        for i in 0..16 {
            src[500 + i] = (token >> (8 * i)) as u8;
        }
        assert_eq!(read_token(&src, 500, 128), Some(token));
        // a byte set beyond the 128 bits we can hold is a token we cannot compare:
        src[500 + 20] = 1;
        assert_eq!(read_token(&src, 500, 128), None);
    }

    #[test]
    fn parse_yaml_chunks() {
        let docs = yaml_rust2::YamlLoader::load_from_str(
            "chunks:\n- [4194312, 100]\n- [8, 20]\n").unwrap();
        let chunks = yaml_hash(&docs[0], "chunks").unwrap();
        assert_eq!(parse_chunks(chunks).unwrap(), vec![(4194312, 100), (8, 20)]);
        let bad = yaml_rust2::YamlLoader::load_from_str("chunks:\n- [8]\n").unwrap();
        assert!(parse_chunks(yaml_hash(&bad[0], "chunks").unwrap()).is_err());
    }

    #[test]
    fn parse_server_token_value() {
        // 2**127 + 1, well past what an i64 can hold:
        let docs = yaml_rust2::YamlLoader::load_from_str(
            "token: 170141183460469231731687303715884105729\ntiny: 12\n").unwrap();
        assert_eq!(yaml_u128(yaml_hash(&docs[0], "token").unwrap()),
                   Some(170141183460469231731687303715884105729));
        assert_eq!(yaml_u128(yaml_hash(&docs[0], "tiny").unwrap()), Some(12));
    }
}
