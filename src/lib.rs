use std::{str};

// The xpra *protocol* version we claim in the hello packet, which is what the server checks for
// compatibility - deliberately not this crate's own version below.
pub const VERSION: &str = "6.4";

// The oldest xpra version we are willing to talk to, sent as the "protocol" capability so that an
// older server can bail out with a clear error instead of failing on the first packet type it does
// not know (`protocol_compat_check`, xpra util/version.py - the server does the same to us). Every
// packet this client sends, and every capability it advertises, uses the names xpra 6.5 introduced;
// nothing here accommodates the pre-6.5 spellings any more.
pub const MIN_PROTOCOL_VERSION: [u32; 2] = [6, 5];

// This client's own version, as reported by `--version`. Read from Cargo.toml so that the package
// version is the single place to bump (the man page header and debian/changelog follow by hand).
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const VERSION_KEY_STR: &str = "version";

pub mod exit_codes;
pub mod net;
