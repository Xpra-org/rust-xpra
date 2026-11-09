use std::{str};

// The oldest xpra version we are willing to talk to, sent as the "protocol-version" capability so
// that an older server can bail out with a clear error instead of failing on the first packet type
// it does not know (`protocol_compat_check`, xpra util/version.py).
// Every packet this client sends, and every capability it advertises, uses the names xpra 6.5
// introduced; nothing here accommodates the pre-6.5 spellings any more. The packets themselves only
// need 6.6 (`window-ack`, the settled `clipboard-data` layout), but 7.0 is the oldest server this
// client is tested against and supports, so that is the floor it announces.
pub const MIN_PROTOCOL_VERSION: [u32; 2] = [7, 0];

// This client's own version, sent in the hello packet and reported by `--version`. Read from
// Cargo.toml so that the package version is the single place to bump (the man page header and
// debian/changelog follow by hand).
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const VERSION_KEY_STR: &str = "version";

pub mod exit_codes;
pub mod net;
