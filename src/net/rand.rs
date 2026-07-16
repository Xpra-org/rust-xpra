// OS cryptographically-secure random bytes, with a hex convenience wrapper. Used for the challenge
// client-salt and padding (see the client's answer_challenge); the hex form is ASCII, so it survives
// our JSON-as-YAML writer intact.
use log::warn;

use super::sha256::to_hex;

// `nbytes` of OS randomness, hex-encoded (so 2*nbytes ASCII chars).
pub fn secure_hex(nbytes: usize) -> String {
    let mut buf = vec![0u8; nbytes];
    secure_random_bytes(&mut buf);
    to_hex(&buf)
}

pub fn secure_random_bytes(buf: &mut [u8]) {
    #[cfg(unix)]
    {
        use std::fs::File;
        use std::io::Read;
        if File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(buf))
            .is_ok()
        {
            return;
        }
        warn!("/dev/urandom unavailable, using a non-cryptographic salt fallback");
    }
    #[cfg(windows)]
    {
        // ProcessPrng (bcryptprimitives) is the modern system CSPRNG - a single buffer, no handle,
        // and documented never to fail; it is what the Rust stdlib and getrandom use on Windows.
        use windows::Win32::Security::Cryptography::ProcessPrng;
        if unsafe { ProcessPrng(buf) }.as_bool() {
            return;
        }
        warn!("ProcessPrng failed, using a non-cryptographic salt fallback");
    }
    // Last resort (the OS CSPRNG effectively never fails): a time/address-seeded splitmix64.
    // Adequate only to keep the client salt unique/unpredictable-enough to avoid replay; it never
    // touches the password itself, which is HMAC'd regardless.
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut state = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (buf.as_ptr() as u64);
    for b in buf.iter_mut() {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        *b = (z ^ (z >> 31)) as u8;
    }
}
